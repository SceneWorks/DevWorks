//! Native Help over the real production gateway, local bundle and maintained documentation.
//! These tests run headlessly; no remote HTTP or learned inference is permitted.
mod common;
use common::*;
use devworks_core::protocol::records::{HelpProvenance, HelpReadView};
use devworks_core::settings::catalog::Category;
use devworks_desktop::{
    backend::Backend,
    help_ops as hops,
    route::{Route, SCREENS},
};
use eframe::egui::{self, accesskit::Role};
use egui_kittest::kittest::{NodeT, Queryable};
use std::sync::Arc;
type Ui = egui_kittest::Harness<'static, devworks_desktop::shell::Shell>;
fn open(engine: &devworks_desktop::host::Engine, root: &Root) -> Ui {
    let mut h = harness(shell(engine, root, Script::default()), 1600.0, 1100.0);
    settle(&mut h);
    click_nav(&mut h, "Help");
    ready(&mut h);
    assert!(h.state().help.loaded, "real Help service answered");
    assert!(h.state().help.error.is_none(), "{:?}", h.state().help.error);
    h
}
fn ready(h: &mut Ui) {
    step_until(h, "real initial Help publication", |shell| {
        !shell.help.topics.is_empty() && !shell.help.generating
    });
    settle(h);
}
fn press(h: &mut Ui, label: &str) {
    click(h, Role::Button, label);
}
fn replace(h: &mut Ui, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    settle(h);
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    if text.is_empty() {
        h.key_press(egui::Key::Backspace);
    } else {
        h.get_by_role_and_label(Role::TextInput, label)
            .type_text(text);
    }
    h.step();
    h.step();
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, label)
            .accesskit_node()
            .value()
            .as_deref(),
        Some(text)
    );
}
fn selected(h: &Ui) -> (&str, &str) {
    match h
        .state()
        .help
        .read
        .as_ref()
        .expect("selected native Help read")
    {
        HelpReadView::Available { topic, passage, .. } => {
            assert!(!passage.text.0.is_empty());
            (topic.id.as_str(), topic.version.0.as_str())
        }
        other => panic!("expected available exact topic, got {other:?}"),
    }
}
#[test]
fn native_every_registered_screen_opens_its_exact_local_help_topic() {
    let root = Root::new("help-all-screen-contexts");
    let engine = start(&root);
    let mut h = open(&engine, &root);
    for screen in &SCREENS {
        h.state_mut().navigate(screen.route);
        settle(&mut h);
        press(&mut h, "Help for current screen");
        assert_eq!(h.state().route(), Route::Help);
        assert_eq!(selected(&h).0, screen.help_topic, "screen {}", screen.id);
        assert!(
            matches!(h.state().help.read, Some(HelpReadView::Available { ref topic, .. }) if matches!(topic.provenance, Some(HelpProvenance::Bundled { .. })))
        );
        assert!(
            h.query_by_role_and_label(Role::TextInput, "Search local Help")
                .is_some()
        );
    }
}
#[test]
fn native_settings_sections_open_exact_topics_and_keep_unsaved_settings() {
    let root = Root::new("help-settings-context");
    let engine = start(&root);
    let mut h = open(&engine, &root);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    click(&mut h, Role::Button, Category::Power.label());
    click(
        &mut h,
        Role::CheckBox,
        "Keep the computer awake while DevWorks is open",
    );
    let drafts = h.state().settings.drafts();
    assert!(!drafts.is_empty());
    for category in Category::ALL {
        h.state_mut().navigate(Route::Settings);
        settle(&mut h);
        click(&mut h, Role::Button, category.label());
        press(&mut h, &format!("Help for {}", category.label()));
        assert_eq!(selected(&h).0, category.help_id());
        press(&mut h, "Back to previous screen");
        assert_eq!(h.state().route(), Route::Settings);
        assert_eq!(h.state().settings.category, Some(category));
        assert_eq!(
            h.state().settings.drafts(),
            drafts,
            "Help never submits/discards a settings draft"
        );
    }
    click(&mut h, Role::Button, "Provider connections");
    press(&mut h, "Help for Provider connections");
    assert_eq!(selected(&h).0, "settings-connections");
    press(&mut h, "Back to previous screen");
    assert!(h.state().settings.show_connections);
    assert_eq!(h.state().settings.drafts(), drafts);
}
#[test]
fn native_help_search_and_bounded_passages_use_local_version_references() {
    let root = Root::new("help-offline-search");
    let offline = Arc::new(OfflineHttp(std::sync::atomic::AtomicUsize::new(0)));
    let mut config = config(&root.0);
    config.http = Some(offline.clone());
    let engine = devworks_desktop::host::Engine::start(config).unwrap();
    let b = engine.backend();
    let mut h = open(&engine, &root);
    replace(&mut h, "Search local Help", "routing");
    press(&mut h, "Search Help");
    assert!(h.state().help.error.is_none());
    assert!(!h.state().help.topics.is_empty());
    let topic = h.state().help.topics[0].clone();
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    assert_eq!(selected(&h), (topic.id.as_str(), topic.version.0.as_str()));
    let view = hops::passage(b.query(
        "help.read",
        &hops::read(&*b, topic.id.as_str(), &topic.version.0, None),
    ))
    .unwrap();
    let HelpReadView::Available {
        passage, snapshot, ..
    } = view
    else {
        panic!("real local passage")
    };
    assert!(passage.text.0.chars().count() <= hops::PASSAGE_CHARS as usize);
    assert!(has_text(&h, &format!("App {}", snapshot.app_version.0)));
    assert!(has_text(
        &h,
        &format!("Topic {} · version {}", topic.id.as_str(), topic.version.0)
    ));
    let query = h.state().help.query.clone();
    h.state_mut().navigate(Route::Chat);
    settle(&mut h);
    click_nav(&mut h, "Help");
    ready(&mut h);
    assert_eq!(h.state().help.query, query);
    assert_eq!(selected(&h), (topic.id.as_str(), topic.version.0.as_str()));
    assert_eq!(
        offline.0.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "offline Help never asks a provider or remote search transport"
    );
}
#[test]
fn native_missing_context_is_explicit_and_never_becomes_an_empty_success() {
    let root = Root::new("help-missing-context");
    let engine = start(&root);
    let mut h = open(&engine, &root);
    h.state_mut().open_help_context("not-a-real-screen");
    settle(&mut h);
    assert!(h.state().help.notice.is_some() || h.state().help.error.is_some());
    assert!(h.state().help.topics.is_empty());
    assert!(has_text(&h, "Missing") || has_text(&h, "unavailable") || has_text(&h, "Refused"));
}
#[test]
fn native_help_generation_unknown_outcome_replays_the_exact_frame_after_relaunch() {
    let root = Root::new("help-generation-unknown");
    let engine = start(&root);
    let real = engine.backend();
    let lossy = LoseAcks::over(real.clone(), "help.generate_capability_docs", 4);
    let mut h = harness(
        shell_with(Ok(lossy.clone()), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Help");
    ready(&mut h);
    assert!(h.state().help.snapshot.as_ref().unwrap().can_generate);
    press(&mut h, "Regenerate capability documentation");
    assert!(h.state().help.generating && h.state().help.generation_unknown);
    assert!(has_text(&h, "Generation outcome not confirmed"));
    assert_eq!(h.state().unconfirmed_count(), 1);
    let frames = lossy.frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 4);
    assert!(frames.iter().all(|frame| frame == &frames[0]));
    h.state_mut().on_exit();
    let saved = devworks_desktop::memory::Memory::load(&root.memory());
    assert_eq!(saved.pending.commands.len(), 1);
    assert_eq!(saved.pending.commands[0].frame, frames[0]);
    drop(h);
    // Startup automatically confirms saved outcomes with their original frames. Keep that
    // first confirmation uncertain too, then explicitly confirm without creating a new key.
    let again = LoseAcks::over(real, "help.generate_capability_docs", 4);
    let mut h = harness(
        shell_with(Ok(again.clone()), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    assert!(h.state().help.generation_unknown);
    press(&mut h, "Confirm outcomes");
    assert!(!h.state().help.generation_unknown);
    assert_eq!(h.state().unconfirmed_count(), 0);
    let confirmed = again.frames.lock().unwrap();
    assert_eq!(confirmed.len(), 5);
    assert!(confirmed.iter().all(|frame| frame == &frames[0]));
}

struct HelpOnly {
    inner: Arc<dyn Backend>,
    runtime: Arc<devworks_engine::Runtime>,
    context: devworks_core::context::InvocationContext,
    queries: std::sync::Mutex<Vec<String>>,
    commands: std::sync::Mutex<Vec<String>>,
}
impl Backend for HelpOnly {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        self.queries.lock().unwrap().push(operation.into());
        self.runtime.query(&self.context, operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.commands.lock().unwrap().push(operation.into());
        self.runtime.command(&self.context, operation, frame)
    }
    fn mint_workspace_upload(
        &self,
        _: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        Err("read-only Help fixture".into())
    }
    fn mint_project_upload(
        &self,
        _: &devworks_core::protocol::ProjectId,
        _: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        Err("read-only Help fixture".into())
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
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
    fn read_stream(&self, _: &devworks_core::protocol::Id, _: u64) -> Result<Vec<u8>, String> {
        Err("Help passages must not request artifact authority".into())
    }
}
fn help_only(engine: &devworks_desktop::host::Engine) -> Arc<HelpOnly> {
    use devworks_core::context::{HostIngress, PrincipalKind};
    use devworks_core::ports::storage::{
        NewPrincipalBinding, StorageError, WriteOptions, run_write,
    };
    use devworks_core::protocol::{ActorId, Id};
    let runtime = engine.host().runtime().clone();
    let store = runtime.store().clone();
    let user = engine.host().user().clone();
    let actor = ActorId(Id::new("help-only-reviewer").unwrap());
    let who = actor.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        tx.actors()
            .register(&who, PrincipalKind::Agent, "Help reviewer")?;
        devworks_core::authority::service::bind_principal(
            tx,
            &user,
            NewPrincipalBinding {
                actor: who,
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::CodeReviewer),
                project_id: None,
                tool_families: Some(vec!["help".into()]),
                bound_by: user.principal().clone(),
            },
            None,
        )
        .map(|_| ())
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))
    })
    .unwrap();
    let user = engine.host().user();
    let context = HostIngress::new(&*store).context(
        actor,
        PrincipalKind::Agent,
        Id::new("help-only-invocation").unwrap(),
        Id::new("help-only-native-view").unwrap(),
        user.restore_epoch().clone(),
        user.policy_revision().clone(),
    );
    Arc::new(HelpOnly {
        inner: engine.backend(),
        runtime,
        context,
        queries: Default::default(),
        commands: Default::default(),
    })
}
#[test]
fn native_read_only_help_needs_no_artifact_or_addon_authority_and_cannot_generate() {
    let root = Root::new("help-native-read-only");
    let engine = start(&root);
    let backend = help_only(&engine);
    let mut h = harness(
        shell_with(Ok(backend.clone()), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Help");
    ready(&mut h);
    let snapshot = h
        .state()
        .help
        .snapshot
        .as_ref()
        .expect("actual read-only snapshot")
        .clone();
    assert!(!snapshot.can_generate);
    assert!(snapshot.cannot_generate_reason.is_some());
    assert!(
        h.get_by_role_and_label(Role::Button, "Regenerate capability documentation")
            .accesskit_node()
            .is_disabled()
    );
    let topic = h
        .state()
        .help
        .topics
        .first()
        .expect("Help-only browsing")
        .clone();
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    assert_eq!(selected(&h).0, topic.id.as_str());
    assert!(backend.commands.lock().unwrap().is_empty());
    let queries = backend.queries.lock().unwrap();
    assert!(!queries.iter().any(|op| op == "artifacts.read"
        || op == "addons.catalog"
        || op == "history.operation_status"));
    drop(queries);
    let denied = hops::generated(backend.command(
        "help.generate_capability_docs",
        &hops::generate(
            &*backend,
            snapshot.registry_revision.get(),
            snapshot.help_revision.get(),
        ),
    ));
    assert!(
        matches!(&denied, Err(devworks_desktop::ops::Failure::Rejected(code)) if code == "PermissionDenied"),
        "the real gateway also rejects a direct read-only bypass before admission: {:?}",
        denied.as_ref().err()
    );
}

#[test]
fn native_help_navigation_is_first_class_and_unavailable_runtime_is_explicit() {
    let root = Root::new("help-runtime-unavailable");
    let mut h = harness(
        shell_with(
            Err("Local runtime could not open".into()),
            &root,
            Script::default(),
            None,
        ),
        1100.0,
        800.0,
    );
    settle(&mut h);
    assert!(nav(&h, "Help").rect().top() < nav(&h, "Settings").rect().top());
    click_nav(&mut h, "Help");
    assert_eq!(h.state().route(), Route::Help);
    assert!(has_text(&h, "Help service unavailable"));
    assert!(has_text(&h, "Local runtime could not open"));
    assert!(
        h.get_by_role_and_label(Role::Button, "Search Help")
            .accesskit_node()
            .is_disabled()
    );
    assert!(h.state().help.topics.is_empty());
}

#[test]
fn native_help_pages_search_results_and_reads_the_complete_maintained_source_example() {
    let root = Root::new("help-native-pages");
    let engine = start(&root);
    let mut h = open(&engine, &root);
    let before = h.state().help.topics.len();
    assert!(
        h.state().help.next_cursor.is_some(),
        "the real bundled catalog exceeds one native page"
    );
    press(&mut h, "More Help topics");
    assert!(h.state().help.topics.len() > before);
    let unique = h
        .state()
        .help
        .topics
        .iter()
        .map(|topic| (topic.id.as_str(), topic.version.0.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), h.state().help.topics.len());
    replace(&mut h, "Search local Help", "echo");
    press(&mut h, "Search Help");
    let topic = h
        .state()
        .help
        .topics
        .iter()
        .find(|topic| topic.id.as_str() == "sdk-echo-source")
        .expect("maintained full SDK source guide")
        .clone();
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    let Some(HelpReadView::Available { passage, .. }) = &h.state().help.read else {
        panic!("first real passage")
    };
    assert_eq!(passage.offset.get(), 0);
    assert_eq!(passage.text.0.chars().count(), hops::PASSAGE_CHARS as usize);
    assert!(passage.has_more);
    let first = passage.text.0.clone();
    press(&mut h, "Next Help passage");
    let Some(HelpReadView::Available { passage, .. }) = &h.state().help.read else {
        panic!("second real passage")
    };
    assert_eq!(passage.offset.get(), hops::PASSAGE_CHARS as i64);
    assert!(!passage.has_more);
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/help/topics/sdk-echo-source.md"),
    )
    .unwrap();
    assert_eq!(
        format!("{first}{}", passage.text.0),
        source,
        "all maintained source bytes remain available through bounded local passages"
    );
    press(&mut h, "Previous Help passage");
    assert!(
        matches!(&h.state().help.read, Some(HelpReadView::Available { passage, .. }) if passage.offset.get() == 0 && passage.text.0 == first)
    );
}

#[test]
fn native_notification_preferences_help_keeps_the_unsaved_preference_draft() {
    let root = Root::new("help-notification-preferences");
    let engine = start(&root);
    let mut h = open(&engine, &root);
    h.state_mut().navigate(Route::Settings);
    settle(&mut h);
    press(&mut h, "Notifications");
    click(&mut h, Role::DisclosureTriangle, "Notification preferences");
    replace(&mut h, "Notification kinds", "Completion");
    let draft = serde_json::to_value(&h.state().decisions.config_draft).unwrap();
    press(&mut h, "Help for Notification preferences");
    assert_eq!(selected(&h).0, "settings-Notifications");
    assert_eq!(
        serde_json::to_value(&h.state().decisions.config_draft).unwrap(),
        draft
    );
    press(&mut h, "Back to previous screen");
    assert_eq!(h.state().route(), Route::Settings);
    press(&mut h, "Notifications");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, "Notification kinds")
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Completion")
    );
}

fn wait_generated(
    b: &dyn Backend,
    operation: &str,
) -> devworks_core::protocol::records::HelpSnapshot {
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        let page = hops::topics(b.query("help.search", &hops::search(b, "", None))).unwrap();
        if let Some(status) = &page.snapshot.generation_operation
            && status.operation_id.0.as_str() == operation
        {
            match status.state {
                devworks_core::protocol::OperationState::ObservedComplete => return page.snapshot,
                devworks_core::protocol::OperationState::Failed
                | devworks_core::protocol::OperationState::CancelledBeforeDispatch
                | devworks_core::protocol::OperationState::NeedsInput => {
                    panic!("real generation did not complete: {:?}", status.state)
                }
                _ => {}
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Help generation {operation} never completed; latest snapshot: {:?}",
            page.snapshot
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
#[test]
fn native_help_regeneration_reports_actual_completion_and_registration_facts() {
    let root = Root::new("help-real-generation");
    let engine = start(&root);
    let b = engine.backend();
    let mut h = open(&engine, &root);
    press(&mut h, "Regenerate capability documentation");
    let id = h
        .state()
        .help
        .generation_operation
        .clone()
        .expect("actual durable generation reference");
    let complete = wait_generated(&*b, &id);
    press(&mut h, "Refresh Help");
    assert!(!h.state().help.generating);
    assert!(has_text(&h, "Capability documentation status: Complete"));
    assert_eq!(
        h.state().help.snapshot.as_ref().unwrap().help_revision,
        complete.help_revision
    );
    replace(&mut h, "Search local Help", "help.search");
    press(&mut h, "Search Help");
    let topic = h
        .state()
        .help
        .topics
        .iter()
        .find(|topic| {
            topic
                .capabilities
                .0
                .iter()
                .any(|capability| capability.operation.0 == "help.search")
        })
        .expect("generated exact operation reference")
        .clone();
    let capability = topic
        .capabilities
        .0
        .iter()
        .find(|capability| capability.operation.0 == "help.search")
        .unwrap()
        .clone();
    assert!(capability.available, "actual Help search registration");
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    click(&mut h, Role::DisclosureTriangle, "Capability reference");
    assert!(has_text(&h, "Registered"));
    assert!(has_text(&h, &capability.authority.0));
    assert!(has_text(&h, capability.input_schema_digest.as_str()));
    assert!(has_text(&h, "Each request still checks its target"));
}
#[test]
fn native_stale_generation_refusal_keeps_the_inspected_version_and_unsaved_search() {
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let root = Root::new("help-generation-stale");
    let engine = start(&root);
    let real = engine.backend();
    let gate = Gate::new("help.generate_capability_docs").missing_hints();
    let mut h = harness(
        shell_with(Ok(gate.over(real.clone())), &root, Script::default(), None),
        1600.0,
        1100.0,
    );
    settle(&mut h);
    click_nav(&mut h, "Help");
    ready(&mut h);
    let topic = h.state().help.topics[0].clone();
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    let before = h.state().help.snapshot.as_ref().unwrap().clone();
    gate.close();
    let _release = Release(gate.clone());
    h.get_by_role_and_label(Role::Button, "Regenerate capability documentation")
        .click();
    step_until(&mut h, "native generation request captured", |shell| {
        shell.help.generating
    });
    // A user can keep typing while the previously captured request waits for its reply.
    h.get_by_role_and_label(Role::TextInput, "Search local Help")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, "Search local Help")
        .type_text("unsaved search draft");
    h.step();
    h.step();
    assert_eq!(h.state().help.query, "unsaved search draft");
    // An unchanged regeneration may legitimately retain its content revision. Change the
    // actual signed source, then publish it, while the older native request remains captured.
    install_guide(
        &engine,
        &root,
        "1.0.0",
        "# Changed local source\n\nA real installed guide changes this catalog.\n",
        false,
    );
    let after = hops::topics(real.query("help.search", &hops::search(&*real, "*", None)))
        .unwrap()
        .snapshot;
    assert_ne!(
        after.help_revision, before.help_revision,
        "real generation advances its CAS token"
    );
    gate.open();
    settle(&mut h);
    let notice = &h
        .state()
        .help
        .generation_notice
        .as_ref()
        .expect("refused captured request")
        .1;
    assert!(notice.contains("Refused"), "{notice}");
    assert!(!h.state().help.generating);
    assert_eq!(h.state().help.query, "unsaved search draft");
    assert_eq!(selected(&h), (topic.id.as_str(), topic.version.0.as_str()));
    assert_eq!(
        h.state().help.snapshot.as_ref().unwrap().help_revision,
        after.help_revision
    );
}

struct OfflineHttp(std::sync::atomic::AtomicUsize);
impl devworks_core::ports::http::HttpTransport for OfflineHttp {
    fn send(
        &self,
        _: &devworks_core::ports::http::HttpRequest,
    ) -> Result<devworks_core::ports::http::HttpResponse, devworks_core::ports::http::HttpError>
    {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(devworks_core::ports::http::HttpError::Unreachable(
            "offline Help fixture".into(),
        ))
    }
}

fn install_guide(
    engine: &devworks_desktop::host::Engine,
    root: &Root,
    version: &str,
    help: &str,
    update: bool,
) -> String {
    use devworks_core::fingerprint::sha256_hex;
    use devworks_core::protocol::results::OperationResult;
    use devworks_desktop::{addons, ops};
    use serde_json::json;
    let b = engine.backend();
    // The fixture disables the package after installation. Inert bytes cannot run a valid helper;
    // this fixture qualifies installed, signed Help source independently of runtime readiness.
    let helper = b"inert documentation fixture, not an executable";
    let entry = if cfg!(windows) {
        "bin/helper.exe"
    } else {
        "bin/helper"
    };
    let manifest = json!({"packageId":"local-help-fixture","version":version,"publisherId":"help-test-publisher","protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},"minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],"schemaAssets":[],"requestedLimits":{"maxConcurrent":1,"queueCapacity":4,"maxMemoryBytes":"1048576","streamCreditBytes":4096},"targets":[{"targetTriple":devworks_adapters::packages::host_target_triple(),"entrypoint":entry}],"files":[{"path":entry,"sha256":sha256_hex(helper),"byteLength":helper.len().to_string()},{"path":"help.md","sha256":sha256_hex(help.as_bytes()),"byteLength":help.len().to_string()}],"stateSchema":{"schemaId":"local-help-fixture/state","revision":"1","sha256":"a".repeat(64)},"configurationSchema":{"schemaId":"local-help-fixture/config","revision":"1","sha256":"b".repeat(64)},"helpFile":"help.md","requestedPermissions":[],"contributions":[{"kind":"Tool","meta":{"id":"local-help-fixture/inspect","inputSchema":{"schemaId":"local-help-fixture/in","revision":"1","sha256":"c".repeat(64)},"outputSchema":{"schemaId":"local-help-fixture/out","revision":"1","sha256":"d".repeat(64)},"helpTopic":"help","required":false,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]}).to_string();
    let key = ed25519_dalek::SigningKey::from_bytes(&[29; 32]);
    let bundle = devworks_adapters::packages::write_bundle(
        manifest.as_bytes(),
        &[(entry, helper.as_slice()), ("help.md", help.as_bytes())],
        Some(&key),
    );
    use devworks_core::ports::packages::PackageInspector;
    let inspected = devworks_adapters::packages::BundleInspector
        .inspect(&bundle)
        .unwrap();
    assert!(
        inspected.inspection.signer_key.is_some(),
        "actual native bundle signature verification"
    );
    let manifest_digest = inspected.inspection.manifest_digest;
    let path = root.file(&format!("local-help-fixture-{version}.dwpkg"), &bundle);
    let ticket = b.mint_workspace_upload(&path).unwrap();
    let artifact = ops::saved_artifact(b.command(
        "artifacts.save",
        &ops::save_workspace_artifact(
            &*b,
            ticket.as_str(),
            "Signed local guide",
            "application/vnd.devworks.package",
            &sha256_hex(&bundle),
            bundle.len() as u64,
            &[],
        ),
    ))
    .unwrap();
    let (_, revision, _) =
        addons::catalog(b.query("addons.catalog", &addons::catalog_frame(&*b, None))).unwrap();
    let (op, frame) = addons::install_saved(
        &*b,
        &artifact.artifact,
        update.then_some("local-help-fixture"),
        revision,
    );
    let installed = match ops::command_result(b.command(op, &frame)).unwrap() {
        OperationResult::AddonsInstall { value } | OperationResult::AddonsUpdate { value } => value,
        other => panic!("actual signed package installation: {other:?}"),
    };
    assert_eq!(
        installed.installed,
        Some(true),
        "exact source was actually installed: {installed:?}"
    );
    if installed.enabled {
        let (_, registry, _) =
            addons::catalog(b.query("addons.catalog", &addons::catalog_frame(&*b, None))).unwrap();
        ops::command_result(b.command("addons.disable", &addons::lifecycle_frame(&*b,json!({"kind":"Disable","packageId":installed.id,"expectedRegistryRevision":registry.to_string()})))).unwrap();
    }
    assert_eq!(installed.digest.as_str(), manifest_digest);
    // Installation changes the registry, not its published documentation. Await the real
    // background owner and its completed source; do not race it with another mutation.
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        let page = hops::topics(b.query(
            "help.package_guides",
            &hops::guides(&*b, "local-help-fixture", version, None),
        ))
        .unwrap();
        if page.items.0.iter().any(|topic| {
            matches!(&topic.provenance,
            Some(HelpProvenance::Package {package_version, manifest_digest: digest, ..})
                if package_version.0 == version && digest.as_str() == manifest_digest)
        }) && page
            .snapshot
            .generation_operation
            .as_ref()
            .is_some_and(|operation| {
                matches!(
                    operation.state,
                    devworks_core::protocol::OperationState::ObservedComplete
                )
            })
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "installed guide never published: {page:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    manifest_digest
}
#[test]
fn native_installed_package_help_has_exact_unicode_passages_and_stale_version_controls() {
    let root = Root::new("help-installed-package");
    let engine = start(&root);
    let first = format!(
        "# Local Unicode guide\n\nOffline guide marker.\n{}\nEnd of first guide.\n",
        "段😀".repeat(9000)
    );
    let digest = install_guide(&engine, &root, "1.0.0", &first, false);
    let mut h = open(&engine, &root);
    // The native app uses egui's default fallback box for uninstalled glyphs. The harness
    // defaults to panicking instead; keep the exact Unicode text to test passage boundaries.
    h.ctx
        .set_missing_glyph_policy(egui::MissingGlyphPolicy::Tofu);
    replace(&mut h, "Search local Help", "Offline guide marker");
    press(&mut h, "Search Help");
    let topic = h.state().help.topics.iter().find(|topic| matches!(&topic.provenance,Some(HelpProvenance::Package {package_id,..}) if package_id.as_str()=="local-help-fixture")).expect("installed package guide is discoverable with Help search").clone();
    assert!(
        matches!(&topic.provenance,Some(HelpProvenance::Package {package_version,manifest_digest,..}) if package_version.0=="1.0.0" && manifest_digest.as_str()==digest)
    );
    press(
        &mut h,
        &format!(
            "Read Help topic {} version {}",
            topic.id.as_str(),
            topic.version.0
        ),
    );
    let Some(HelpReadView::Available { passage, .. }) = &h.state().help.read else {
        panic!("installed current passage")
    };
    assert_eq!(passage.text.0.chars().count(), hops::PASSAGE_CHARS as usize);
    let prefix = passage.text.0.clone();
    press(&mut h, "Next Help passage");
    let Some(HelpReadView::Available { passage, .. }) = &h.state().help.read else {
        panic!("Unicode continuation")
    };
    assert_eq!(passage.offset.get(), hops::PASSAGE_CHARS as i64);
    assert_eq!(
        format!("{prefix}{}", passage.text.0),
        first,
        "scalar offsets never split or replace UTF-8"
    );
    press(&mut h, "All guides for this package");
    assert!(h.state().help.topics.iter().all(|topic| matches!(&topic.provenance,Some(HelpProvenance::Package {package_id,package_version,..}) if package_id.as_str()=="local-help-fixture" && package_version.0=="1.0.0")));
    let second = "# Updated local guide\n\nOffline guide marker, version two.\n";
    let next_digest = install_guide(&engine, &root, "2.0.0", second, true);
    press(&mut h, "Refresh Help");
    assert!(
        matches!(&h.state().help.read,Some(HelpReadView::Unavailable {availability:devworks_core::protocol::records::HelpAvailability::Stale,requested_version,current_version:Some(current),..}) if requested_version.0=="1.0.0" && current.0=="2.0.0")
    );
    press(&mut h, "Open installed version 2.0.0");
    assert!(
        matches!(&h.state().help.read,Some(HelpReadView::Available {topic,passage,..}) if passage.text.0==second && matches!(&topic.provenance,Some(HelpProvenance::Package {package_version,manifest_digest,..}) if package_version.0=="2.0.0" && manifest_digest.as_str()==next_digest))
    );
}

#[test]
fn native_context_help_returns_to_the_same_conversation_and_unsent_draft() {
    let root = Root::new("help-unsent-conversation");
    let engine = start(&root);
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1100.0);
    settle(&mut h);
    replace(
        &mut h,
        devworks_desktop::chat::COMPOSER_LABEL,
        "Keep this unsent draft while reading Help",
    );
    let conversation = h.state().chat.selected.clone();
    press(&mut h, "Help for current screen");
    step_until(&mut h, "published contextual Help passage", |shell| {
        shell.help.read.is_some()
    });
    settle(&mut h);
    assert_eq!(selected(&h).0, "chat");
    press(&mut h, "Back to previous screen");
    assert_eq!(h.state().route(), Route::Chat);
    assert_eq!(h.state().chat.selected, conversation);
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, devworks_desktop::chat::COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep this unsent draft while reading Help")
    );
}
