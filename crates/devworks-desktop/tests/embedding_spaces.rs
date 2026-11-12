//! DW119: actual native Settings → durable candidate → accounted recorded probe → selection.
//! The provider returns recorded vectors; no learned model or network runs in this fixture.
mod common;
use common::*;
use devworks_desktop::ops;
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};
use std::sync::Arc;
#[path = "embedding_spaces/fixture.rs"]
mod fixture;
use fixture::*;

fn setup_qualify_reopen(paid: bool, lose_acks: bool) {
    let root = Root::new(if paid {
        "embedding-spaces-paid"
    } else {
        "embedding-spaces-free"
    });
    let http = Arc::new(RecordedEmbedding::default());
    let engine = configured(&root, http.clone());
    let backend = engine.backend();
    let seen = LoseAcks::over(backend.clone(), "router.configure", 0);
    let mut h = harness(
        shell_with(Ok(seen.clone()), &root, Script::default(), None),
        1500.0,
        1100.0,
    );
    create_candidate(&mut h, &http, paid);
    let page = || {
        ops::embedding_space_page(backend.query(
            "router.embedding_spaces",
            &ops::router_embedding_spaces(&*backend, None),
        ))
        .unwrap()
    };
    let before = page();
    assert_eq!(
        before.items.0.len(),
        1,
        "candidate creation UI: message={:?}, error={:?}, frames={:?}",
        h.state().settings.connections.message,
        h.state().settings.connections.embedding_spaces.error,
        seen.frames
            .lock()
            .unwrap()
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect::<Vec<_>>()
    );
    assert!(before.selected_candidate_id.is_none());
    assert_eq!(
        serde_json::to_value(&before.items.0[0].status).unwrap(),
        "Unqualified"
    );
    if paid {
        let model = before.items.0[0]
            .pricing_model_id
            .as_ref()
            .expect("actual price-only subject");
        assert!(
            !h.get_by_role_and_label(Role::Button, "Configure price for Recorded space")
                .accesskit_node()
                .is_disabled(),
            "pricing link must be actionable: loading={}, busy={:?}",
            h.state().settings.connections.embedding_spaces.loading,
            h.state().settings.connections.busy
        );
        click(&mut h, Role::Button, "Configure price for Recorded space");
        let edit = format!("Edit tariff override for {}", model.as_str());
        assert!(
            h.query_by_role_and_label(Role::Button, &edit).is_some(),
            "actual price subject navigation: buttons={:?}, labels={:?}",
            labels(&h, Role::Button),
            labels(&h, Role::Label)
        );
        click(&mut h, Role::Button, &edit);
        assert_eq!(
            h.get_by_role_and_label(Role::TextInput, "Tariff upstream model")
                .accesskit_node()
                .value()
                .as_deref(),
            Some("fixture-embedding")
        );
        type_into(&mut h, "Amount USD per rate", "1");
        click(&mut h, Role::Button, "Save tariff override");
        assert!(
            http.requests.lock().unwrap().is_empty(),
            "pricing does not run a probe"
        );
        click_nav(&mut h, "Settings");
        // Returning to Settings retains the explicitly opened connection/setup panel.
        settle(&mut h);
    }
    let lossy = if lose_acks {
        // Close the first shell as the app does. Its periodic memory save can hold the candidate
        // command from while it was in flight; a bare drop leaves that stale entry, and the next
        // shell would resubmit it (into the lost acknowledgments) instead of sending qualify.
        h.state_mut().on_exit();
        assert!(
            devworks_desktop::memory::Memory::load(&root.memory())
                .pending
                .commands
                .is_empty(),
            "the closed shell leaves no command to resubmit"
        );
        drop(h);
        let lossy = LoseAcks::over(backend.clone(), "router.configure", 4);
        h = harness(
            shell_with(Ok(lossy.clone()), &root, Script::default(), None),
            1500.0,
            1100.0,
        );
        settle(&mut h);
        click_nav(&mut h, "Settings");
        click(&mut h, Role::Button, "Provider connections");
        click(&mut h, Role::Button, "Configure embedding spaces");
        Some(lossy)
    } else {
        None
    };
    click(
        &mut h,
        Role::Button,
        "Queue qualification for Recorded space (may incur usage)",
    );
    if let Some(lossy) = lossy {
        assert_eq!(h.state().unconfirmed_count(), 1);
        let frames = lossy.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 4);
        assert!(
            frames.iter().all(|f| f == &frames[0]),
            "unknown acknowledgment retries exactly the original admitted frame"
        );
        // The command receipt can commit before the background qualification intent becomes
        // visible to a separate read snapshot. Wait for that exact durable operation rather
        // than assuming the UI's last repaint also advanced the worker.
        let deadline = std::time::Instant::now() + SETTLE_BOUND;
        let original_operation = loop {
            let current = page();
            if let Some(operation) = current.items.0[0].qualification_operation.as_ref() {
                break operation.operation_id.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "qualification intent was not recorded: {current:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        h.state_mut().on_exit();
        let saved = devworks_desktop::memory::Memory::load(&root.memory());
        assert_eq!(saved.pending.commands.len(), 1);
        assert_eq!(saved.pending.commands[0].frame, frames[0]);
        drop(h);
        let replay = LoseAcks::over(backend.clone(), "router.configure", 0);
        h = harness(
            shell_with(Ok(replay.clone()), &root, Script::default(), None),
            1500.0,
            1100.0,
        );
        settle(&mut h);
        assert_eq!(h.state().unconfirmed_count(), 0);
        assert_eq!(*replay.frames.lock().unwrap(), vec![frames[0].clone()]);
        assert_eq!(
            page().items.0[0]
                .qualification_operation
                .as_ref()
                .unwrap()
                .operation_id,
            original_operation
        );
        click_nav(&mut h, "Settings");
        click(&mut h, Role::Button, "Provider connections");
        click(&mut h, Role::Button, "Configure embedding spaces");
    }

    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    let qualified = loop {
        let p = page();
        let status = serde_json::to_value(&p.items.0[0].status).unwrap();
        if status != "Qualifying" {
            break p;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "qualification closes: {p:?}"
        );
        std::thread::yield_now();
    };
    assert_eq!(
        serde_json::to_value(&qualified.items.0[0].status).unwrap(),
        "Qualified",
        "{qualified:?}"
    );
    click(&mut h, Role::Button, "Reload embedding spaces");
    click(&mut h, Role::Button, "Use embedding space Recorded space");
    let selected = page();
    assert_eq!(
        selected.selected_candidate_id.as_ref(),
        Some(&selected.items.0[0].id)
    );
    let retained = selected.items.0[0].clone();
    assert!(retained.qualification_attempt_id.is_some());
    assert!(retained.result_artifact.is_some());
    let usage_body = serde_json::json!({"reportKind":"Attempts","scope":{"kind":"Workspace"},"page":{"limit":100}});
    let usage_frame = devworks_desktop::usage_ops::query(&*backend, "router.usage", usage_body);
    let usage =
        devworks_desktop::usage_ops::usage(backend.query("router.usage", &usage_frame)).unwrap();
    let devworks_core::protocol::records::UsageQueryResult::Attempts { page: attempts } = usage
    else {
        panic!("actual attempt report")
    };
    let actual = attempts
        .items
        .0
        .iter()
        .find(|a| a.attempt_id == retained.qualification_attempt_id)
        .expect("the qualified native attempt is accounted");
    assert_eq!(actual.input_tokens.as_ref().map(|n| n.as_str()), Some("7"));
    assert!(
        !actual.unknown_tail,
        "a positive qualification requires closed accounting"
    );
    assert_eq!(actual.request_id, retained.qualification_request_id);
    assert_eq!(
        actual.cost.amount_micro_usd.as_ref().map(|n| n.as_str()),
        Some(if paid { "7" } else { "0" })
    );
    assert!(
        actual.cost.monetary_complete,
        "the explicit cost source is accounted completely"
    );
    let requests = http.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body["model"], "fixture-embedding");
    assert_eq!(
        body["input"],
        serde_json::json!([devworks_core::services::router::embedding_spaces::PROBE_TEXT])
    );
    assert!(
        body.get("dimensions").is_none(),
        "Native dimensions are not a wire parameter"
    );
    drop(requests);
    drop(h);
    drop(seen);
    drop(backend);
    drop(engine);
    let reopened = configured(&root, http.clone());
    let backend = reopened.backend();
    let restored = ops::embedding_space_page(backend.query(
        "router.embedding_spaces",
        &ops::router_embedding_spaces(&*backend, None),
    ))
    .unwrap();
    assert_eq!(restored.selected_candidate_id, Some(retained.id));
    assert_eq!(restored.items.0[0].space_id, retained.space_id);
    assert_eq!(
        restored.items.0[0].qualification_attempt_id,
        retained.qualification_attempt_id
    );
    assert_eq!(
        http.requests.lock().unwrap().len(),
        1,
        "reopen never resends a probe"
    );
    if !paid && !lose_acks {
        let records = || {
            ops::connection_records(backend.query(
                "router.connections",
                &ops::router_connections(&*backend, None),
            ))
            .unwrap()
            .0
        };
        let current = records().remove(0);
        let original_endpoint = current.endpoint.as_ref().unwrap().0.clone();
        ops::configured(backend.command("router.configure", &ops::router_configure(&*backend,serde_json::json!({"kind":"Connection","connectionId":current.id,"expectedRevision":current.revision,"name":"Renamed embeddings","profile":"Embedding","endpoint":original_endpoint})))).unwrap();
        let inspect = || {
            ops::embedding_space_page(backend.query(
                "router.embedding_spaces",
                &ops::router_embedding_spaces(&*backend, None),
            ))
            .unwrap()
        };
        let renamed = inspect();
        assert_eq!(
            renamed.items.0[0].space_id, restored.items.0[0].space_id,
            "display edits do not relabel vector identity"
        );
        assert_eq!(
            serde_json::to_value(&renamed.items.0[0].status).unwrap(),
            "Qualified"
        );
        assert_eq!(
            renamed.items.0[0].qualification_attempt_id,
            restored.items.0[0].qualification_attempt_id
        );
        let current = records().remove(0);
        ops::configured(backend.command("router.configure", &ops::router_configure(&*backend,serde_json::json!({"kind":"Connection","connectionId":current.id,"expectedRevision":current.revision,"name":"Changed destination","profile":"Embedding","endpoint":"http://localhost:11434/another-endpoint"})))).unwrap();
        let stale = inspect();
        assert_eq!(
            serde_json::to_value(&stale.items.0[0].status).unwrap(),
            "Stale"
        );
        assert!(
            !stale.items.0[0].can_qualify,
            "an immutable candidate cannot be requalified as a different encoder"
        );
        assert!(
            stale.items.0[0].pricing_model_id.is_none(),
            "old price metadata is not current owner authority"
        );
        assert_eq!(
            stale.items.0[0].space_id, restored.items.0[0].space_id,
            "old proof remains inspectable"
        );
        let request = ops::router_configure(
            &*backend,
            serde_json::json!({"kind":"QualifyEmbeddingSpace","candidateId":stale.items.0[0].id,"expectedRevision":stale.items.0[0].revision}),
        );
        let Err(ops::Failure::Refused(fault)) =
            ops::command_result(backend.command("router.configure", &request))
        else {
            panic!("changed source must refuse new paid work")
        };
        assert!(matches!(
            fault.code,
            devworks_core::protocol::DomainErrorCode::UnsupportedCapability
        ));
        assert_eq!(
            http.requests.lock().unwrap().len(),
            1,
            "identity changes never silently route, requalify or resend"
        );
    }
}

#[test]
fn settings_qualifies_selects_and_reopens_an_explicit_recorded_embedding_space() {
    setup_qualify_reopen(false, false);
}

#[test]
fn settings_prices_an_embedding_subject_before_accounted_paid_qualification() {
    setup_qualify_reopen(true, false);
}

#[test]
fn unknown_qualification_ack_replays_the_original_frame_operation_and_native_request() {
    setup_qualify_reopen(false, true);
}

#[test]
fn retained_embedding_controls_survive_an_actual_background_read() {
    let root = Root::new("embedding-retained-read");
    let http = Arc::new(RecordedEmbedding::default());
    let engine = configured(&root, http.clone());
    let backend = engine.backend();
    let gate = Gate::holding_replies("router.embedding_spaces");
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let mut h = harness(
        shell_with(
            Ok(gate.over(backend.clone())),
            &root,
            Script::default(),
            None,
        ),
        1500.0,
        1500.0,
    );
    let _release = Release(gate.clone());
    create_candidate(&mut h, &http, false);
    h.get_by_role_and_label(Role::Button, "Configure price for Recorded space")
        .scroll_to_me();
    settle(&mut h);
    assert!(
        h.state()
            .settings
            .connections
            .embedding_spaces
            .page
            .is_some()
    );
    let connection = ops::connection_records(backend.query(
        "router.connections",
        &ops::router_connections(&*backend, None),
    ))
    .unwrap()
    .0
    .remove(0);
    gate.close();
    // A real committed change emits the normal runtime hint. Hold only the subsequent read
    // reply, so the loading interval covers the entire pointer interaction deterministically.
    ops::configured(backend.command("router.configure", &ops::router_configure(&*backend,serde_json::json!({"kind":"Connection","connectionId":connection.id,"expectedRevision":connection.revision,"name":"Background renamed connection","profile":"Embedding","endpoint":connection.endpoint.unwrap().0})))).unwrap();
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while !h.state().settings.connections.embedding_spaces.loading {
        assert!(
            std::time::Instant::now() < deadline,
            "the actual background read starts"
        );
        h.step();
        std::thread::yield_now();
    }
    let panel = &h.state().settings.connections.embedding_spaces;
    assert!(
        panel.page.is_some(),
        "the inspected candidate remains retained during the read"
    );
    assert!(
        !h.get_by_role_and_label(Role::TextInput, "Embedding space name")
            .accesskit_node()
            .is_disabled(),
        "background reads cannot disable a retained configuration draft"
    );
    assert!(
        !h.get_by_role_and_label(Role::Button, "Configure price for Recorded space")
            .accesskit_node()
            .is_disabled(),
        "background reads cannot eat the user's pointer click"
    );
    h.get_by_role_and_label(Role::Button, "Configure price for Recorded space")
        .click();
    h.step();
    assert_eq!(
        h.state().route(),
        devworks_desktop::route::Route::Usage,
        "the real pointer action navigates while the independent read is held"
    );
    gate.open();
    settle(&mut h);
    assert!(
        http.requests.lock().unwrap().is_empty(),
        "a metadata refresh or price navigation never runs paid inference"
    );
}
