use super::*;
use devworks_core::ports::{
    storage::{
        ReadOptions, StoredPriceSnapshot, StoredPriceStatus, WriteOptions, read_snapshot, run_write,
    },
    usage::{CostClassification, PriceQuery, PriceResolver, PublishedTariff},
};
use devworks_core::protocol::Id;
use devworks_core::services::prices::CatalogPriceResolver;

#[test]
fn native_official_tariff_edit_preserves_selected_snapshot_dimensions_and_quotes() {
    let root = Root::new("usage-native-official-edit");
    let mut config = config(&root.0);
    config.connection_tick = None;
    let engine = devworks_desktop::host::Engine::start(config).unwrap();
    let b = engine.backend();
    ops::configured(b.command("router.configure", &ops::router_configure(&*b,json!({"kind":"Connection","name":"Official quote connection","profile":"OpenAiResponses","endpoint":"https://api.openai.com/v1"})))).unwrap();
    let connection =
        ops::connection_records(b.query("router.connections", &ops::router_connections(&*b, None)))
            .unwrap()
            .0
            .into_iter()
            .find(|c| c.name.0 == "Official quote connection")
            .unwrap();
    let model = "native-official-edit-model";
    ops::configured(b.command("router.configure", &ops::router_configure(&*b,json!({"kind":"Model","model":{"id":model,"displayName":"Official edit model","endpointProfile":"OpenAiResponses","connectionId":connection.id,"upstreamModelId":"gpt-test","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})))).unwrap();
    let base = PublishedTariff {
        upstream_model_id: "gpt-test".into(),
        service_tier: "Standard".into(),
        context_lower_exclusive: None,
        context_upper_inclusive: Some(400_000),
        modality: "Text".into(),
        charge_kind: "Input".into(),
        cache_ttl_seconds: None,
        unit: "Token".into(),
        units: 1_000_000,
        amount_usd: "1".into(),
        region: None,
        endpoint_profile: Some("OpenAiResponses".into()),
    };
    let mut tariffs = vec![base.clone()];
    for (kind, amount, ttl) in [
        ("Output", "2", None),
        ("CacheRead", "0.1", None),
        ("CacheWrite", "0.2", Some(3600)),
    ] {
        tariffs.push(PublishedTariff {
            charge_kind: kind.into(),
            amount_usd: amount.into(),
            cache_ttl_seconds: ttl,
            ..base.clone()
        });
    }
    tariffs.push(PublishedTariff {
        modality: "Image".into(),
        unit: "Image".into(),
        units: 1,
        amount_usd: "3".into(),
        ..base.clone()
    });
    let expected = tariffs.clone();
    // Two actual immutable snapshots, including an unrelated upstream in the selected one.
    // The GUI must copy only this model's current snapshot, even with history loaded.
    let mut unrelated = base.clone();
    unrelated.upstream_model_id = "gpt-unrelated".into();
    unrelated.amount_usd = "999".into();
    tariffs.push(unrelated);
    let store = engine.host().runtime().store();
    run_write(&**store, WriteOptions::default(), move |tx| {
        for (number, id) in [(1, "official-old"), (2, "official-selected")] {
            let previous = tx.usage().price_status("openai")?;
            let status = StoredPriceStatus {
                provider: "openai".into(),
                revision: previous.as_ref().map_or(1, |s| s.revision + 1),
                current_snapshot_id: previous
                    .as_ref()
                    .and_then(|s| s.current_snapshot_id.clone()),
                last_attempt_ms: Some(tx.now_ms()),
                last_success_ms: previous.as_ref().and_then(|s| s.last_success_ms),
                refresh_pending: true,
                refresh_operation_id: previous
                    .as_ref()
                    .and_then(|s| s.refresh_operation_id.clone()),
                last_error: None,
                next_refresh_ms: Some(i64::MAX),
            };
            tx.usage()
                .put_price_status(&status, previous.as_ref().map(|s| s.revision))?;
            let mut rows = tariffs.clone();
            if number == 1 {
                rows[0].amount_usd = "99".into();
            }
            let snapshot = StoredPriceSnapshot {
                id: Id::new(id).unwrap(),
                provider: "openai".into(),
                revision: number,
                source_url: "https://developers.openai.com/api/docs/pricing".into(),
                source_digest: format!("{number:064x}"),
                parser_version: "fixture-official".into(),
                fetched_at_ms: tx.now_ms(),
                verified_at_ms: tx.now_ms(),
                effective_date: None,
                tariffs: rows,
            };
            tx.usage()
                .publish_price_snapshot(&snapshot, status.revision)?;
        }
        Ok(())
    })
    .unwrap();
    let query = PriceQuery {
        catalog_model_id: Id::new(model).unwrap(),
        provider_model_id: Some("gpt-test".into()),
        connection_id: connection.id,
        endpoint_profile: "OpenAiResponses".into(),
        access_mode: "ApiKey".into(),
        service_tier: None,
        input_tokens: Some(100),
        cache_write_ttl_ms: Some(3_600_000),
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let quote = || {
        read_snapshot(&**store, &ReadOptions::default(), |read| {
            CatalogPriceResolver.quote(read, &query)
        })
        .unwrap()
    };
    let original = quote();
    assert_eq!(
        original.classification,
        CostClassification::OfficialEstimate
    );
    assert_eq!(
        original.snapshot_id.as_ref().unwrap().as_str(),
        "official-selected"
    );
    let mut h = harness(shell(&engine, &root, Script::default()), 1800.0, 1800.0);
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    click(&mut h, Role::CheckBox, "Include historical price sources");
    press(&mut h, "Apply price catalog filters");
    press(&mut h, &format!("Edit tariff override for {model}"));
    let draft = h.state().usage.price_draft.as_ref().unwrap();
    assert_eq!(draft.revision, "1");
    assert_eq!(
        draft.tariffs.len(),
        expected.len(),
        "every dimension of the selected official snapshot seeds the editor"
    );
    let initial = draft
        .tariffs
        .iter()
        .map(|t| t.tariff().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(&initial).unwrap(),
        serde_json::to_value(
            price_catalog(&*b, model)
                .items
                .0
                .iter()
                .filter(|e| matches!(e.origin, PriceOrigin::OfficialSnapshot)
                    && e.snapshot_id
                        .as_ref()
                        .is_some_and(|id| id.as_str() == "official-selected")
                    && e.upstream_model_id.0 == "gpt-test")
                .map(|e| e.tariff.clone())
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
    let first = h
        .query_all_by_role_and_label(Role::TextInput, "Amount USD per rate")
        .next()
        .unwrap();
    first.scroll_to_me();
    settle(&mut h);
    h.query_all_by_role_and_label(Role::TextInput, "Amount USD per rate")
        .next()
        .unwrap()
        .focus();
    h.step();
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.query_all_by_role_and_label(Role::TextInput, "Amount USD per rate")
        .next()
        .unwrap()
        .type_text("1.75");
    h.step();
    press(&mut h, "Save tariff override");
    let saved = price_catalog(&*b, model);
    assert_eq!(saved.models.0[0].revision.as_str(), "2");
    let configured = saved
        .items
        .0
        .iter()
        .filter(|row| {
            matches!(row.origin, PriceOrigin::UserConfigured)
                && row.model_id.as_ref().is_some_and(|id| id.as_str() == model)
                && row
                    .override_revision
                    .as_ref()
                    .is_some_and(|r| r.as_str() == "2")
        })
        .collect::<Vec<_>>();
    assert_eq!(configured.len(), expected.len());
    let mut expected_saved = serde_json::to_value(&initial).unwrap();
    expected_saved[0]["amountUsd"] = json!("1.75");
    assert_eq!(
        serde_json::to_value(configured.iter().map(|e| &e.tariff).collect::<Vec<_>>()).unwrap(),
        expected_saved,
        "saving one field preserves every other exact tariff dimension"
    );
    let revised = quote();
    assert_eq!(
        revised.classification,
        CostClassification::ConfiguredEstimate
    );
    assert_eq!(
        revised
            .rates
            .as_ref()
            .unwrap()
            .input_usd_per_million
            .as_deref(),
        Some("1.75")
    );
    let mut remaining_before = original.tariffs.clone();
    let mut remaining_after = revised.tariffs.clone();
    remaining_before.retain(|t| !(t.modality == "Text" && t.charge_kind == "Input"));
    remaining_after.retain(|t| !(t.modality == "Text" && t.charge_kind == "Input"));
    assert_eq!(
        remaining_before, remaining_after,
        "output, cache and additional modality quotes survive an input edit"
    );
    assert_eq!(
        original
            .rates
            .as_ref()
            .unwrap()
            .input_usd_per_million
            .as_deref(),
        Some("1"),
        "captured official quote remains unchanged"
    );
    press(&mut h, &format!("Edit tariff override for {model}"));
    assert_eq!(
        h.state().usage.price_draft.as_ref().unwrap().tariffs.len(),
        expected.len()
    );
    assert_eq!(
        h.state().usage.price_draft.as_ref().unwrap().tariffs[0].amount,
        "1.75",
        "current override takes precedence over official history"
    );
}
