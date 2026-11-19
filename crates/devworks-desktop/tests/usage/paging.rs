use super::*;
use devworks_core::locks::LockExt;

#[test]
fn native_price_more_gesture_survives_same_frame_catalog_hint_without_replacing_partial_page() {
    let _case = native_runtime_case();
    let root = Root::new("usage-price-more-hint");
    let engine = start(&root);
    startup::settled(&engine, &root);
    let b = engine.backend();
    let model = "hint-paged-model";
    ops::configured(b.command("router.configure",&ops::router_configure(&*b, serde_json::from_value(json!({"kind":"Model","model":{"id":model,"displayName":"Hint paged model","endpointProfile":"CompatibleChat","upstreamModelId":"fixture/hint-paged","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})).unwrap()))).unwrap();
    let tariffs=(0..105).map(|i|json!({"upstreamModelId":"fixture/hint-paged","serviceTier":format!("service-{i:03}"),"modality":"Text","chargeKind":"Input","unit":"Token","units":"1000000","amountUsd":"1"})).collect::<Vec<_>>();
    uops::overridden(b.command("router.price_override",&uops::command(&*b, command_input("router.price_override", json!({"modelId":model,"expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}))))).unwrap();
    let (signal, hints) = tokio::sync::watch::channel(0);
    let hinted = std::sync::Arc::new(Hinted {
        inner: b.clone(),
        hints,
    });
    let gate = Gate::new("router.price_status");
    let mut h = harness(
        shell_with(Ok(gate.over(hinted)), &root, Script::default(), None),
        1800.0,
        1800.0,
    );
    settle(&mut h);
    h.state_mut().navigate(Route::Usage);
    settle(&mut h);
    press(&mut h, "Prices");
    // Hold the real writer admission slot before the explicit first read. Native owners may
    // remain queued; WAL readers continue against the same committed event snapshot.
    let _writer = WriterHold::new(engine.host().runtime().store().clone());
    press(&mut h, "Apply price catalog filters");

    let before_rows = h.state().usage.price_view().unwrap().items.0.len();
    assert!(before_rows < 105 && h.state().usage.price_view().unwrap().has_more);
    h.get_by_role_and_label(Role::Button, "Load more price rows")
        .scroll_to_me();
    settle(&mut h);
    let before_generation = h.state().usage.prices.generation();
    let target = h
        .get_by_role_and_label(Role::Button, "Load more price rows")
        .rect();
    assert!(
        !h.get_by_role_and_label(Role::Button, "Load more price rows")
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
    let sequence = devworks_core::ports::storage::read_snapshot(
        &**engine.host().runtime().store(),
        &Default::default(),
        |read| Ok(read.stamp().committed_sequence),
    )
    .unwrap();
    // Hints mean "may have changed": duplicates/progress wakeups do not imply a new event.
    // Control only this host signal; every price query/command still uses the real runtime.
    signal.send_modify(|v| *v += 1);
    h.get_by_role_and_label(Role::Button, "Load more price rows")
        .click();
    h.step();
    // Only the cursor append started (one request more): a background revalidation did not
    // start a first-page read that would replace the loaded page set in the gesture frame.
    let append_generation = h.state().usage.prices.generation();
    assert_eq!(
        append_generation,
        before_generation + 1,
        "background revalidation cannot supersede the cursor's loaded page set in the gesture frame"
    );
    assert!(
        h.state()
            .client()
            .unwrap()
            .is_pending(&devworks_desktop::backend::Slot::Usage(
                devworks_desktop::usage_ops::UsageSlot::Prices(append_generation, true)
            )),
        "the single pointer gesture must submit its cursor append"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::Button, "Load more price rows")
            .rect()
            .min,
        target.min,
        "the native target stays in place"
    );
    gate.open();
    settle(&mut h);
    assert_eq!(h.state().usage.price_view().unwrap().items.0.len(), 105);
    assert_eq!(h.state().usage.prices.pages(), 2);
    assert_eq!(
        devworks_core::ports::storage::read_snapshot(
            &**engine.host().runtime().store(),
            &Default::default(),
            |read| Ok(read.stamp().committed_sequence)
        )
        .unwrap(),
        sequence,
        "the continuation used the identical authoritative snapshot"
    );
}

#[test]
fn native_price_re_read_after_a_saved_override_keeps_the_pages_load_more_added() {
    let _case = native_runtime_case();
    let root = Root::new("usage-price-reread-pages");
    let engine = start(&root);
    startup::settled(&engine, &root);
    let b = engine.backend();
    let model = "reread-paged-model";
    ops::configured(b.command("router.configure",&ops::router_configure(&*b, serde_json::from_value(json!({"kind":"Model","model":{"id":model,"displayName":"Re-read paged model","endpointProfile":"CompatibleChat","upstreamModelId":"fixture/reread-paged","enabled":true,"tierMemberships":[],"efforts":[],"imageInput":false,"tools":false,"revision":"1"}})).unwrap()))).unwrap();
    let tariffs=(0..105).map(|i|json!({"upstreamModelId":"fixture/reread-paged","serviceTier":format!("service-{i:03}"),"modality":"Text","chargeKind":"Input","unit":"Token","units":"1000000","amountUsd":"1"})).collect::<Vec<_>>();
    uops::overridden(b.command("router.price_override",&uops::command(&*b, command_input("router.price_override", json!({"modelId":model,"expectedRevision":"1","change":{"kind":"Set","tariffs":tariffs}}))))).unwrap();
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
    {
        // The continuation needs one unchanged real event snapshot.
        let _writer = WriterHold::new(engine.host().runtime().store().clone());
        press(&mut h, "Apply price catalog filters");
        assert!(h.state().usage.price_view().unwrap().has_more);
        press(&mut h, "Load more price rows");
        assert_eq!(h.state().usage.price_view().unwrap().items.0.len(), 105);
        assert_eq!(h.state().usage.prices.pages(), 2);
    }
    // Saving an override re-reads the applied catalog: both pages are read again and joined
    // into one view, so the rows (and the editor they allow) stay.
    press(&mut h, &format!("Edit tariff override for {model}"));
    // The re-read's first request waits at the gate until the save's own writes are done and
    // the writer slot is held, so both pages are read at one event snapshot.
    h.get_by_role_and_label(Role::Button, "Save tariff override")
        .scroll_to_me();
    settle(&mut h);
    gate.close();
    h.get_by_role_and_label(Role::Button, "Save tariff override")
        .click();
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while !gate.has_waiter() {
        h.step();
        assert!(std::time::Instant::now() < deadline, "the first request");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let writer = WriterHold::new(engine.host().runtime().store().clone());
    gate.open();
    step_until(&mut h, "the saved override", |s| {
        !s.usage.price_busy && s.usage.price_draft.is_none()
    });
    step_until(&mut h, "the re-read", |s| !s.usage.prices.loading());
    drop(writer);
    assert_eq!(h.state().usage.prices.pages(), 2);
    let view = h.state().usage.price_view().unwrap();
    assert!(!view.has_more);
    assert_eq!(view.items.0.len(), 105);
    assert!(
        h.state().usage.price_error.is_none(),
        "{:?}",
        h.state().usage.price_error
    );
}

struct Hinted {
    inner: std::sync::Arc<dyn Backend>,
    hints: tokio::sync::watch::Receiver<u64>,
}
impl Backend for Hinted {
    fn query(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        self.inner.query(op, frame)
    }
    fn command(&self, op: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.inner.command(op, frame)
    }
    fn mint_workspace_upload(
        &self,
        p: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(p)
    }
    fn mint_project_upload(
        &self,
        id: &devworks_core::protocol::ProjectId,
        p: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(id, p)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.hints.clone())
    }
    fn set_waker(&self, w: devworks_desktop::backend::Waker) {
        self.inner.set_waker(w)
    }
    fn token(&self, p: &str) -> String {
        self.inner.token(p)
    }
    fn scratch_dir(&self) -> std::path::PathBuf {
        self.inner.scratch_dir()
    }
    fn read_stream(&self, id: &devworks_core::protocol::Id, n: u64) -> Result<Vec<u8>, String> {
        self.inner.read_stream(id, n)
    }
}

/// An owned no-op transaction controls this fixture's no-event premise. It never completes a
/// native producer or manufactures a reply; all first/continuation reads still use the gateway.
pub(super) struct WriterHold {
    release: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    cancel: devworks_core::ports::storage::Cancellation,
    worker:
        Option<std::thread::JoinHandle<Result<(), devworks_core::ports::storage::StorageError>>>,
}
impl WriterHold {
    pub(super) fn new(
        store: std::sync::Arc<dyn devworks_core::ports::storage::ApplicationStore>,
    ) -> Self {
        use devworks_core::ports::storage::{Cancellation, StorageError, WriteOptions, run_write};
        let release =
            std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (entered, admitted) = std::sync::mpsc::sync_channel(1);
        let cancel = Cancellation::new();
        let options = WriteOptions {
            cancellation: Some(cancel.clone()),
            ..WriteOptions::fixture()
        };
        let gate = release.clone();
        let worker = std::thread::spawn(move || {
            run_write(&*store, options, move |_| {
                entered.send(()).map_err(|_| StorageError::Cancelled)?;
                let mut open = gate.0.locked();
                while !*open {
                    open = gate.1.wait(open).unwrap_or_else(|p| p.into_inner());
                }
                Ok(())
            })
        });
        let hold = Self {
            release,
            cancel,
            worker: Some(worker),
        };
        admitted
            .recv_timeout(SETTLE_BOUND)
            .expect("the real writer slot is admitted");
        hold
    }
}
impl Drop for WriterHold {
    fn drop(&mut self) {
        self.cancel.cancel();
        *self.release.0.locked() = true;
        self.release.1.notify_one();
        let result = self.worker.take().expect("owned writer worker").join();
        if !std::thread::panicking() {
            assert!(
                matches!(
                    result,
                    Ok(Ok(())) | Ok(Err(devworks_core::ports::storage::StorageError::Cancelled))
                ),
                "writer control finishes: {result:?}"
            );
        }
    }
}
