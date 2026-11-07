//! Actual native maintenance uses the shared Conventions gateway, queue and body index owner.
use super::*;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_core::services::conventions::model;

pub(super) struct DirectNative(pub(super) Arc<Runtime>);
impl NativeJobDispatch for DirectNative {
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> devworks_core::gateway::CommandOutcome {
        self.0.command(ctx, operation, raw)
    }
}
pub(super) fn imported_body(app: &HelpApp, rt: &tokio::runtime::Runtime) -> Id {
    let body = "# Native schedule\n\nRetain the actual source and scope.";
    let manifest = json!({"format":"devworks-convention","version":1,"slug":"native-schedule","title":"Native schedule","parentPath":[],"sortOrder":0,"lifecycle":"Active","applicability":[],"markdown":{"sha256":devworks_core::fingerprint::sha256_hex(body.as_bytes()),"byteLength":body.len().to_string(),"text":body},"resources":[]});
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let artifacts = app.runtime.artifacts().clone();
    let staged = artifacts
        .stage_host_bytes(
            app.store.as_ref(),
            &bytes,
            json!({"kind":"HumanPickedPortableFixture"}),
        )
        .unwrap()
        .unwrap();
    let (a, file, actor) = (
        artifacts.clone(),
        staged.clone(),
        app.user().principal().clone(),
    );
    let version = write(app.store.as_ref(), move |tx| {
        a.commit_host_bytes_in_scope(
            tx,
            &file,
            HostArtifactPublication {
                title: "Picked portable convention",
                media_type: "application/json",
                scope: ArtifactScope::Workspace,
                owner: actor,
                provenance: json!({"kind":"HumanPickedPortableFixture"}),
            },
        )
    })
    .unwrap();
    artifacts.settle_host_bytes(app.store.as_ref(), &staged, true);
    let result = app.command("conventions.import", json!({"scope":{"kind":"Workspace"},"source":version_ref(&version),"schema":model::manifest_schema()}), 3);
    let CommandReply::Recorded { receipt, .. } = result else {
        panic!("{result:?}");
    };
    let DomainDecision::Accepted { operation_id } = receipt.decision else {
        panic!("{:?}", receipt.decision);
    };
    let owner = read(app.store.as_ref(), |s| {
        s.conventions().convention_import(&operation_id)
    })
    .unwrap()
    .convention_id;
    rt.block_on(within("import and body index", async {
        loop {
            app.runtime.dispatcher().run_until_idle().await.unwrap();
            let import_state = read(app.store.as_ref(), |s| {
                s.operations().operation(&operation_id)
            })
            .unwrap();
            assert!(
                !matches!(
                    import_state.state,
                    OperationState::Failed
                        | OperationState::NeedsInput
                        | OperationState::OutcomeUnknown
                ),
                "{import_state:?}"
            );
            let failures = read(app.store.as_ref(), |s| {
                s.operations().operations_for_capability(
                    "conventions.index",
                    &[
                        OperationState::Failed,
                        OperationState::OutcomeUnknown,
                        OperationState::NeedsInput,
                    ],
                    None,
                    10,
                )
            });
            assert!(
                failures.is_empty(),
                "actual body indexing failed: {failures:?}"
            );
            let complete = read(app.store.as_ref(), |s| {
                Ok(s.operations()
                    .operation(&operation_id)?
                    .is_some_and(|o| o.state == OperationState::ObservedComplete)
                    && s.conventions()
                        .convention_index_head(&owner)?
                        .is_some_and(|i| i.status == "LexicalOnly"))
            });
            if complete {
                break;
            }
            tokio::task::yield_now().await;
        }
    }));
    owner
}
#[test]
fn scheduled_current_conventions_index_uses_real_running_job_and_fresh_scope_capture() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    let owner = imported_body(&app, &rt);
    let capability = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .expect("actual shared Conventions command qualified");
    let definition = json!({"kind":"Native","capabilityId":capability.capability_id,"arguments":{"schema":capability.schema,"value":{}}});
    let job = app.create(
        "Index current conventions",
        json!({"kind":"Interval","seconds":"3600"}),
        definition,
        true,
    );
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        Arc::new(DirectNative(app.runtime.clone())),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    h.advance(3_601_000);
    app.tick(&rt, &scheduler);
    let pending = app.runs(&job).pop().unwrap();
    assert_eq!(
        pending.status, "Running",
        "Accepted native index remains an actual Running job, without a Holding waiver: {pending:?}"
    );
    let inner = pending
        .inner_operation_id
        .clone()
        .expect("actual Accepted index operation");
    let operation = read(app.store.as_ref(), |s| s.operations().operation(&inner)).unwrap();
    assert_eq!(
        operation.payload["sourceContext"]["invocationContext"]["jobRun"],
        json!(pending.id)
    );
    let first = app.settle(&rt, &scheduler, &job);
    assert_eq!(first.status, "Succeeded", "{first:?}");
    let frame1 = captured(&first);
    assert_eq!(frame1["body"]["allScope"]["kind"], "Workspace");
    assert!(frame1["body"]["expectedRevision"].is_string());
    let head = read(app.store.as_ref(), |s| {
        s.conventions().convention_index_head(&owner)
    })
    .unwrap();
    assert_eq!(head.status, "LexicalOnly");
    assert_eq!(head.operation_id, inner);
    let current = read(app.store.as_ref(), |s| {
        s.conventions()
            .convention(&owner, None, &[KnowledgeScope::Workspace])
    })
    .unwrap();
    let body: ArtifactRef = serde_json::from_value(current.content_ref.unwrap()).unwrap();
    let changed = app.command("conventions.edit", json!({"conventionId":owner,"expectedRevision":current.revision.to_string(),"content":body,"title":"Updated actual title","scope":{"kind":"Workspace"}}), 3);
    assert!(
        matches!(
            changed,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Applied { .. },
                    ..
                },
                ..
            }
        ),
        "{changed:?}"
    );
    h.advance(3_600_000);
    app.tick(&rt, &scheduler);
    let second = app.settle(&rt, &scheduler, &job);
    assert_eq!(second.status, "Succeeded", "{second:?}");
    let frame2 = captured(&second);
    assert_ne!(
        frame1["body"]["expectedRevision"],
        frame2["body"]["expectedRevision"]
    );
    assert_ne!(first.id, second.id);
    rt.block_on(app.runtime.shutdown());
}

struct LostAcceptedAck {
    runtime: Arc<Runtime>,
    lost: AtomicBool,
    frames: Mutex<Vec<(Vec<u8>, Id)>>,
    receipts: Mutex<Vec<(Option<OperationId>, bool)>>,
}
impl NativeJobDispatch for LostAcceptedAck {
    fn command(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
    ) -> devworks_core::gateway::CommandOutcome {
        self.frames
            .lock()
            .unwrap()
            .push((raw.to_vec(), ctx.invocation_id().clone()));
        let reply = self.runtime.command(ctx, operation, raw)?;
        if let CommandReply::Recorded {
            receipt, replayed, ..
        } = &reply
        {
            self.receipts
                .lock()
                .unwrap()
                .push((receipt.operation_id.clone(), *replayed));
        }
        if self.lost.swap(false, Ordering::SeqCst) {
            let frame: Value = serde_json::from_slice(raw).unwrap();
            return Ok(serde_json::from_value(json!({"kind":"CommandStatusUnknown","protocol":PROTOCOL,"requestKey":frame["requestKey"],"correlationId":"lost-actual-index-ack"})).unwrap());
        }
        Ok(reply)
    }
}
#[test]
fn lost_accepted_convention_job_ack_replays_one_exact_source_operation() {
    let _native_case = crate::common::native_runtime_case();
    let h = Harness::new();
    drop(h.open());
    seed(&h);
    let rt = runtime();
    let app = boot_help(&rt, &h);
    imported_body(&app, &rt);
    let capability = app
        .runtime
        .jobs()
        .catalog()
        .capabilities()
        .into_iter()
        .find(|c| c.operation == "conventions.index")
        .unwrap();
    let job=app.create("Lost index acknowledgement",json!({"kind":"Manual"}),json!({"kind":"Native","capabilityId":capability.capability_id,"arguments":{"schema":capability.schema,"value":{}}}),true);
    let native = Arc::new(LostAcceptedAck {
        runtime: app.runtime.clone(),
        lost: AtomicBool::new(true),
        frames: Mutex::new(Vec::new()),
        receipts: Mutex::new(Vec::new()),
    });
    let scheduler = devworks_engine::Scheduler::new(
        app.store.clone(),
        app.runtime.jobs().clone(),
        native.clone(),
        Arc::new(NoInferencePipeline),
        app.runtime.processes().clone(),
    );
    value(app.command(
        "jobs.run_now",
        json!({"jobId":job,"expectedRevision":"1"}),
        2,
    ));
    app.tick(&rt, &scheduler);
    let run = app.settle(&rt, &scheduler, &job);
    assert_eq!(run.status, "Succeeded", "{run:?}");
    let frames = native.frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0], frames[1]);
    drop(frames);
    let receipts = native.receipts.lock().unwrap();
    assert_eq!(receipts.len(), 2);
    assert!(!receipts[0].1 && receipts[1].1);
    assert_eq!(receipts[0].0, receipts[1].0);
    assert_eq!(receipts[0].0, run.inner_operation_id);
    drop(receipts);
    let operations = read(app.store.as_ref(), |s| {
        s.operations().operations_for_capability(
            "conventions.index",
            &[OperationState::ObservedComplete],
            None,
            500,
        )
    });
    assert_eq!(
        operations
            .iter()
            .filter(
                |op| op.payload["sourceContext"]["invocationContext"]["jobRun"] == json!(run.id)
            )
            .count(),
        1
    );
    rt.block_on(app.runtime.shutdown());
}
