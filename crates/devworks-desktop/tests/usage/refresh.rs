use super::*;

#[test]
fn native_refresh_gesture_survives_same_frame_catalog_revalidation_hint() {
    let root = Root::new("usage-refresh-hint-gesture");
    let http = std::sync::Arc::new(PublicPriceHttp::default());
    let mut c = config(&root.0);
    c.http = Some(http.clone());
    let engine = devworks_desktop::host::Engine::start(c).unwrap();
    let b = engine.backend();
    let gate = Gate::new("router.price_status");
    let mut h = harness(
        shell_with(Ok(gate.over(b.clone())), &root, Script::default(), None),
        1800.0,
        1800.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    click(&mut h, Role::DisclosureTriangle, "Price catalog filters");
    replace(&mut h, "Price source IDs", "openai");
    press(&mut h, "Apply price catalog filters");
    h.get_by_role_and_label(Role::Button, "Refresh official price sources")
        .scroll_to_me();
    settle(&mut h);
    let before = h
        .get_by_role_and_label(Role::Button, "Refresh official price sources")
        .rect();
    assert!(
        !h.get_by_role_and_label(Role::Button, "Refresh official price sources")
            .accesskit_node()
            .is_disabled()
    );
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.open();
        }
    }
    let _release = Release(gate.clone());
    gate.close();
    let mut hints = b.hints().unwrap();
    hints.borrow_and_update();
    project(&*b, "A real unrelated write wakes catalog revalidation");
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while !hints.has_changed().unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "the runtime publishes its actual queue hint"
        );
        std::thread::yield_now();
    }
    // The native pointer gesture targets the enabled control from the rendered frame.
    // The hint is handled before this frame's widgets and starts a retained-catalog query.
    h.get_by_role_and_label(Role::Button, "Refresh official price sources")
        .click();
    h.step();
    assert!(
        h.state().usage.prices_loading,
        "actual background revalidation starts in the click frame"
    );
    let after = h
        .get_by_role_and_label(Role::Button, "Refresh official price sources")
        .rect();
    assert_eq!(
        before.min, after.min,
        "the target did not move beneath the native pointer"
    );
    assert!(
        after.contains(before.center()),
        "the original gesture remains inside the actual control"
    );
    assert!(
        h.state().usage.price_busy || h.state().usage.refresh.is_some(),
        "one enabled gesture must submit refresh even when the retained catalog is revalidating: disabled={}, error={:?}",
        h.get_by_role_and_label(Role::Button, "Refresh official price sources")
            .accesskit_node()
            .is_disabled(),
        h.state().usage.price_error
    );
    gate.open();
    for _ in 0..4 {
        h.step();
        assert!(h.state().wait_idle());
    }
    h.step();
    assert!(
        h.state().usage.refresh.is_some(),
        "the single gesture has an accepted durable receipt"
    );
    http.release();
    wait_for_price_publication(&*b, 1);
}
