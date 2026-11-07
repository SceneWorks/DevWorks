//! A cancelled, already-sent paid job retains actual native custody across a new boot.
use super::super::conventions::DirectNative;
use super::*;
fn attempts(app: &HelpApp, run: &Id) -> Vec<StoredInferenceAttempt> {
    read(app.store.as_ref(), |s| {
        s.usage().inference_attempts(
            &UsageAttemptFilter {
                job_run_id: Some(run.clone()),
                ..Default::default()
            },
            None,
            10,
        )
    })
}
#[test]
fn paid_native_job_cancel_after_send_preserves_unknown_custody_and_never_restarts_http() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let wire = Arc::new(Wire::default());
    let app = boot_paid(&rt, &h, wire.clone());
    let owner = imported_body(&app, &rt);
    configure(&app, &rt);
    rt.block_on(within("qualified body before paid cancellation", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            if read(app.store.as_ref(), |s| {
                s.conventions().convention_index_head(&owner)
            })
            .is_some_and(|i| i.status == "Ready")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let previous = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    let baseline = wire.calls.load(Ordering::SeqCst);
    let capability = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .unwrap();
    let job=app.create("Paid index cancellation",json!({"kind":"Manual"}),json!({"kind":"Native","capabilityId":capability.capability_id,"arguments":{"schema":capability.schema,"value":{}}}),true);
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        Arc::new(DirectNative(app.runtime.clone())),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    let block = Arc::new(BlockedRead::default());
    let _release = ReleaseBlocked(block.clone());
    *wire.next_block.lock().unwrap() = Some(block.clone());
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    rt.block_on(within("actual dispatched native job source", async {
        while !block.entered.load(Ordering::SeqCst) {
            app.runtime.dispatcher().tick().await.unwrap();
            tokio::task::yield_now().await;
        }
    }));
    let running = app.runs(&job).pop().unwrap();
    assert_eq!(running.status, "Running");
    assert!(running.inner_operation_id.is_some());
    let rows = attempts(&app, &running.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "Dispatched");
    assert_eq!(wire.calls.load(Ordering::SeqCst), baseline + 1);
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
    rt.block_on(within(
        "physical close and preserved paid cancellation custody",
        async {
            loop {
                app.runtime.dispatcher().tick().await.unwrap();
                if block.closed.load(Ordering::SeqCst)
                    && read(app.store.as_ref(), |s| {
                        s.usage().attempt_balance(&rows[0].attempt_id)
                    })
                    .is_some_and(|b| b.local_quiescent && b.unknown_tail)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        },
    ));
    let inner_id = running.inner_operation_id.clone().unwrap();
    rt.block_on(within("actual parent Unknown observation", async {
        loop {
            if read(app.store.as_ref(), |s| s.operations().operation(&inner_id))
                .is_some_and(|op| op.state == OperationState::OutcomeUnknown)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    let native_before = read(app.store.as_ref(), |s| {
        s.usage().inference_attempt(&rows[0].attempt_id)
    })
    .unwrap();
    let native_operation = read(app.store.as_ref(), |s| {
        s.operations().operation(&native_before.operation_id)
    })
    .unwrap();
    let preparation = read(app.store.as_ref(), |s| {
        s.operations().operations_for_capability(
            "conventions.embedding_prepare",
            &[
                OperationState::Dispatching,
                OperationState::OutcomeUnknown,
                OperationState::NeedsInput,
            ],
            None,
            500,
        )
    })
    .into_iter()
    .find(|op| op.payload["origin"]["operationId"] == json!(inner_id))
    .unwrap();
    let balance = read(app.store.as_ref(), |s| {
        s.usage().attempt_balance(&rows[0].attempt_id)
    })
    .unwrap();
    assert!(balance.unknown_tail);
    assert!(!balance.monetary_complete);
    let current = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    assert_eq!(
        current.generation_id, previous.generation_id,
        "cancelled paid batch cannot publish a new Ready head"
    );
    assert!(app.runs(&job)[0].cancel_requested);
    assert_ne!(
        read(app.store.as_ref(), |s| s
            .operations()
            .operation(running.inner_operation_id.as_ref().unwrap()))
        .unwrap()
        .state,
        OperationState::ObservedComplete
    );
    rt.block_on(app.runtime.shutdown());
    drop(scheduler);
    drop(app);
    let reopened = boot_paid(&rt, &h, wire.clone());
    rt.block_on(reopened.runtime.dispatcher().run_until_idle())
        .unwrap();
    // Exercise the actual observer after the original run is now from an older boot.
    // Current ineligibility is not evidence that the already-sent effect did not happen.
    rt.block_on(reopened.runtime.dispatcher().observe_unknown())
        .unwrap();
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .operations()
            .operation(&inner_id))
        .unwrap()
        .state,
        OperationState::OutcomeUnknown
    );
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .operations()
            .operation(&native_before.operation_id))
        .unwrap(),
        native_operation
    );
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .usage()
            .inference_attempt(&native_before.attempt_id))
        .unwrap(),
        native_before
    );
    let prep_after = read(reopened.store.as_ref(), |s| {
        s.operations().operation(&preparation.id)
    })
    .unwrap();
    assert_eq!(prep_after.id, preparation.id);
    assert_eq!(prep_after.args_digest, preparation.args_digest);
    assert_eq!(prep_after.state, OperationState::OutcomeUnknown);
    assert_eq!(
        wire.calls.load(Ordering::SeqCst),
        baseline + 1,
        "new boot observes exact old custody, never a new paid send"
    );
    assert_eq!(attempts(&reopened, &running.id).len(), 1);
    let after = read(reopened.store.as_ref(), |s| {
        s.usage().attempt_balance(&rows[0].attempt_id)
    })
    .unwrap();
    assert!(after.unknown_tail && after.local_quiescent);
    assert_eq!(
        after, balance,
        "same exact balance remains owned by original paid attempt"
    );
    assert_eq!(
        read(reopened.store.as_ref(), |s| s
            .conventions()
            .convention_index_head(&owner))
        .unwrap()
        .generation_id,
        previous.generation_id
    );
    rt.block_on(reopened.runtime.shutdown());
}
