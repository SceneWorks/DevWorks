//! Cancellation reaches the real accepted body-index source, without touching other work.
use super::super::conventions::{DirectNative, imported_body};
use super::*;

#[test]
fn cancelling_an_accepted_convention_job_preserves_previous_head_and_cancels_its_source() {
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let gate = PublishGate::new(&h);
    let app = boot_help_with_files(&rt, &h, gate.clone());
    let owner = imported_body(&app, &rt);
    let old = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    let native = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .unwrap();
    let job = app.create("Cancel actual convention index", json!({"kind":"Manual"}), json!({"kind":"Native","capabilityId":native.capability_id,"arguments":{"schema":native.schema,"value":{}}}), true);
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        Arc::new(DirectNative(app.runtime.clone())),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    let unrelated = enqueue(
        app.store.as_ref(),
        "unrelated-index-control",
        None,
        json!({"retained":true}),
    );
    gate.arm();
    let release = Release(gate.clone());
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    rt.block_on(within("actual convention source verification", async {
        while !gate.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }));
    let running = app.runs(&job).pop().unwrap();
    assert_eq!(running.status, "Running");
    let inner = running.inner_operation_id.as_ref().unwrap();
    let operation = read(app.store.as_ref(), |s| s.operations().operation(inner)).unwrap();
    let delivery: Id = serde_json::from_value(operation.payload["deliveryId"].clone()).unwrap();
    let cancel = app.command(
        "jobs.cancel",
        json!({"jobId":job,"runId":running.id,"expectedRevision":running.revision.to_string()}),
        2,
    );
    assert!(
        matches!(
            cancel,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Accepted { .. },
                    ..
                },
                ..
            }
        ),
        "{cancel:?}"
    );
    assert!(
        read(app.store.as_ref(), |s| s.queue().entry(&delivery))
            .unwrap()
            .cancel_requested
    );
    assert_eq!(entry(app.store.as_ref(), &unrelated.id), unrelated);
    gate.release();
    drop(release);
    let cancelled = app.settle(&rt, &scheduler, &job);
    assert_eq!(cancelled.status, "Cancelled", "{cancelled:?}");
    let after = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    assert_eq!(old.generation_id, after.generation_id);
    assert_eq!(old.source_digest, after.source_digest);
    rt.block_on(app.runtime.shutdown());
}
