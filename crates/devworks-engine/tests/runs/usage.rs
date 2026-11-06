//! Positive managed-turn custody, including assignment-less coordinator inference.
use super::*;
use devworks_core::ports::{
    router::{ConnectionDirectory, ConnectionState, ConnectionStatus},
    usage::{CostClassification, PriceQuery, PriceQuote},
};
use devworks_core::protocol::records::{EffortName, RouteIntent};
use devworks_core::services::{
    router::{
        InferenceRouter, Purpose, RequestAdmission,
        accounting::{AttemptSelection, ReportCategory},
    },
    usage::{
        accounting::{AttemptRequest, DispatchIdentity, Observation},
        host::{DriveOutcome, InferenceFrame, OwnedInferenceSource},
        meter::{InputSemantics, ObservationSemantics, Quantities},
        pricing::PricingContext,
    },
};
use devworks_engine::usage::UsageRuntime;
struct Directory;
impl ConnectionDirectory for Directory {
    fn connections(&self, _: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError> {
        Ok(vec![ConnectionStatus {
            id: id("usage-connection"),
            profile: "CompatibleChat".into(),
            state: ConnectionState::Connected,
            discovered: Some(["native-metadata-source".into()].into_iter().collect()),
            allowed: vec![],
            excluded: vec![],
        }])
    }
}
struct Cleanup {
    runtime: Arc<UsageRuntime>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.runtime.stop();
        let (lock, changed) = &*self.gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
}
struct Source {
    started: Option<tokio::sync::oneshot::Sender<()>>,
    closing: Option<tokio::sync::oneshot::Sender<()>>,
    close_gate: Arc<(Mutex<bool>, Condvar)>,
}
impl OwnedInferenceSource for Source {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        self.started.take().unwrap().send(()).unwrap();
        Ok(())
    }
    fn next(&mut self, cancel: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        let (lock, changed) = &*self.close_gate;
        let mut open = lock.lock().unwrap();
        while !cancel.is_cancelled() {
            let (next, _) = changed
                .wait_timeout(open, Duration::from_millis(5))
                .unwrap();
            open = next;
        }
        Ok(None)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        let _ = self.closing.take().unwrap().send(());
        let (lock, changed) = &*self.close_gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = changed.wait(open).unwrap();
        }
        Ok(())
    }
}
fn router(f: &Fx) -> Arc<InferenceRouter> {
    write(&*f.store, |tx| {
        tx.router().put_connection(
            &StoredConnection {
                id: id("usage-connection"),
                name: "Explicit free metadata fixture".into(),
                profile: "CompatibleChat".into(),
                endpoint: "https://example.test".into(),
                enabled: true,
                local_free: true,
                open_router: None,
                credential_ref: None,
                credential_revision: 0,
                credential_epoch: None,
                lifecycle: "Connected".into(),
                state_reason: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: None,
                checked_at_ms: None,
                revision: 1,
                created_at_ms: T0,
                updated_at_ms: T0,
            },
            None,
        )?;
        tx.router().put_route_model(
            &StoredRouteModel {
                id: id("usage-model"),
                display_name: "Native metadata source".into(),
                endpoint_profile: "CompatibleChat".into(),
                upstream_model_id: "native-metadata-source".into(),
                enabled: true,
                efforts: vec!["Low".into(), "High".into()],
                image_input: false,
                tools: true,
                context_tokens: Some(128000),
                connection_id: Some(id("usage-connection")),
                position: 1,
                revision: 1,
                updated_at_ms: T0,
            },
            None,
        )
    })
    .unwrap();
    Arc::new(InferenceRouter::new(
        f.h.ids.clone(),
        f.h.clock.clone(),
        Arc::new(Directory),
    ))
}
fn job_run(f: &Fx, running: bool) -> Id {
    f.tx(move |_, tx| {
        tx.jobs().insert_job(&StoredJob {
            id: id("feature-job"),
            revision: 1,
            project_id: Some(ProjectId(id("project-a"))),
            title: "Feature work provenance".into(),
            kind: "Inference".into(),
            schedule: r#"{"kind":"Manual"}"#.into(),
            timezone: "UTC".into(),
            definition: "{}".into(),
            enabled: false,
            authorized: true,
            authorized_by: Some(ActorId(id("user-a"))),
            hold: None,
            activated_epoch: 0,
            project_epoch: 0,
            anchor_at_ms: T0,
            next_at_ms: None,
            interrupted_run_id: None,
            created_at_ms: T0,
            updated_at_ms: T0,
            deleted_at_ms: None,
        })?;
        let op = OperationId(id("feature-job-operation"));
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: None,
            assignment_id: None,
            capability: "jobs.run".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        let boot_id = if running {
            tx.store().boot_id.clone()
        } else {
            None
        };
        tx.jobs().insert_run(&StoredJobRun {
            id: id("feature-job-run"),
            job_id: id("feature-job"),
            seq: 1,
            project_id: Some(ProjectId(id("project-a"))),
            job_revision: 1,
            definition: "{}".into(),
            project_epoch: 0,
            restore_epoch: 0,
            occurrence_key: "completed-feature-run".into(),
            occurrence_at_ms: T0,
            trigger: "Manual".into(),
            status: if running {
                "Running".into()
            } else {
                "Succeeded".into()
            },
            revision: 1,
            operation_id: Some(op),
            inner_operation_id: None,
            boot_id,
            started_at_ms: Some(T0),
            finished_at_ms: if running { None } else { Some(T0) },
            reason: None,
            hold: None,
            cancel_requested: false,
            resumptions: 0,
            session_id: None,
            agent_actor: None,
            skipped_for: None,
            checkpoint: None,
            created_at_ms: T0,
        })?;
        tx.jobs()
            .link_root(&id("feature-job-run"), &RootId(id("root-f")))?;
        Ok(id("feature-job-run"))
    })
}
struct NeverOpened;
impl OwnedInferenceSource for NeverOpened {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        panic!("unowned source opened")
    }
    fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        unreachable!()
    }
    fn close(&mut self) -> Result<(), StorageError> {
        unreachable!()
    }
}

struct PanickingSource;
impl OwnedInferenceSource for PanickingSource {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        Ok(())
    }
    fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        panic!("scripted native metadata source failure")
    }
    fn close(&mut self) -> Result<(), StorageError> {
        panic!("unwound source did not close")
    }
}

/// A manual Human chat has monetary attribution but no managed-execution custody.
struct ManualChatSource {
    first: bool,
    observed: Option<tokio::sync::oneshot::Sender<()>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    fail_close: bool,
}
impl OwnedInferenceSource for ManualChatSource {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        Ok(())
    }
    fn next(&mut self, cancel: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        if self.first {
            self.first = false;
            return Ok(Some(InferenceFrame::Usage(Observation {
                key: "manual-usage".into(),
                sequence: 1,
                semantics: ObservationSemantics::Cumulative,
                quantities: Quantities {
                    input_tokens: Some(10),
                    output_tokens: Some(0),
                    ..Default::default()
                },
                reported_cost: None,
                provider_model_id: None,
            })));
        }
        if let Some(observed) = self.observed.take() {
            let _ = observed.send(());
        }
        if self.fail_close {
            return Ok(None);
        }
        let (lock, changed) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released && !cancel.is_cancelled() {
            released = changed
                .wait_timeout(released, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        Ok(None)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        if self.fail_close {
            Err(StorageError::Busy)
        } else {
            Ok(())
        }
    }
}

#[test]
fn actual_coordinator_and_worker_transports_hold_quiescence_until_owned_close() {
    runtime().block_on(async {
        within("managed inference closure", async {
            for (worker, bounded_shutdown, membership, panic_source, manual_failed_close) in [
                (false, false, false, false, None),
                (true, false, false, false, None),
                (false, true, false, false, None),
                (false, false, true, false, None),
                (false, false, false, true, None),
                (false, false, false, false, Some(false)),
                (false, false, false, false, Some(true)),
            ] {
                let f = Fx::new();
                let selected_router = router(&f);
                let physical_child = std::env::var_os("DW21_CAPTURE_OWNER").is_some();
                if physical_child || manual_failed_close.is_some() {
                    f.tx(|_, tx| {
                        let mut connection =
                            tx.router().connection(&id("usage-connection"))?.unwrap();
                        connection.local_free = false;
                        connection.revision += 1;
                        tx.router().put_connection(&connection, Some(1))?;
                        let tariffs = ["Input", "Output"]
                            .map(|kind| devworks_core::ports::usage::PublishedTariff {
                                upstream_model_id: "native-metadata-source".into(),
                                service_tier: "Standard".into(),
                                context_lower_exclusive: None,
                                context_upper_inclusive: None,
                                modality: "Text".into(),
                                charge_kind: kind.into(),
                                cache_ttl_seconds: None,
                                unit: "Token".into(),
                                units: 1_000_000,
                                amount_usd: if kind == "Input" {
                                    "1".into()
                                } else {
                                    "2".into()
                                },
                                region: None,
                                endpoint_profile: Some("CompatibleChat".into()),
                            })
                            .to_vec();
                        tx.usage().put_price_override(
                            &StoredPriceOverride {
                                model_id: id("usage-model"),
                                revision: 2,
                                mode: "Set".into(),
                                tariffs,
                                updated_by: ActorId(id("user-a")),
                                updated_at_ms: T0,
                            },
                            1,
                        )
                    });
                }
                let execution_root = if membership { "root-i" } else { "root-f" };
                let destination = if membership {
                    applied(&f.start(&f.user, "root-g", 1, 1));
                    f.launch("destination-supervisor");
                    Some(f.assign("root-g", "root-g", None, None))
                } else {
                    None
                };
                let app = if bounded_shutdown {
                    let mut composed = composition_with(
                        &f,
                        None,
                        Arc::new(FakePower::default()),
                        Arc::new(Authority),
                        Arc::new(devworks_adapters::process::NativeProcessHost),
                    );
                    composed.use_cases = UseCases::new().with_router(selected_router.clone());
                    composed.config.shutdown_bound = Duration::from_millis(100);
                    Some(boot(composed).await.unwrap())
                } else {
                    None
                };
                assert!(matches!(
                    decision(&f.start(&f.user, execution_root, 1, 1)),
                    DomainDecision::Applied { .. }
                ));
                let activation = f.launch("usage-supervisor");
                let coordinator = f.assign(execution_root, execution_root, None, None);
                let assignment = if worker {
                    f.assign(execution_root, "task-f1", Some(&coordinator.id), None)
                } else {
                    coordinator
                };
                let a = assignment.clone();
                let execution = activation.clone();
                let turn = f.tx(move |service, tx| {
                    service
                        .begin_turn(
                            tx,
                            &a.session_id,
                            &a.id,
                            &execution.id,
                            execution.owner_epoch,
                        )
                        .map(|v| v.unwrap())
                });
                let info = f.store.info();
                let context = devworks_core::context::HostIngress::new(&*f.store)
                    .context(
                        info.host_actor,
                        PrincipalKind::Host,
                        id("inference-invocation"),
                        id("native-worker"),
                        devworks_core::protocol::Counter::new(info.restore_epoch).unwrap(),
                        Revision::new(1).unwrap(),
                    )
                    .with_lineage(
                        Some(ProjectId(id("project-a"))),
                        Some(RootId(id(execution_root))),
                        None,
                    )
                    .with_session(assignment.session_id.clone());
                let query = PriceQuery {
                    catalog_model_id: id("usage-model"),
                    provider_model_id: Some("native-metadata-source".into()),
                    connection_id: id("usage-connection"),
                    endpoint_profile: "CompatibleChat".into(),
                    access_mode: "host-derived".into(),
                    service_tier: None,
                    input_tokens: Some(10),
                    cache_write_ttl_ms: None,
                    processing_region: None,
                    required_modalities: vec!["Text".into()],
                };
                let mut request = AttemptRequest {
                    admission_key: id("owned-admission"),
                    request_id: id("owned-request"),
                    actor_id: context.principal().clone(),
                    work_id: Some(assignment.work_id.0.clone()),
                    job_run_id: worker.then(|| job_run(&f, false)),
                    role: "Host".into(),
                    query,
                    quote: PriceQuote {
                        classification: CostClassification::LocalFree,
                        connection_revision: Some(1),
                        snapshot_id: None,
                        price_revision: None,
                        override_revision: None,
                        currency: "USD".into(),
                        rates: None,
                        tariffs: vec![],
                        untracked_reason: None,
                    },
                    price_generation: 1,
                    override_revision: 1,
                    pricing: PricingContext {
                        input_semantics: InputSemantics::Inclusive,
                        cache_read_possible: false,
                        cache_write_possible: false,
                        cache_write_ttl_ms: None,
                    },
                    selection: AttemptSelection::Conversation {
                        intent: RouteIntent::Model {
                            model_id: id("usage-model"),
                        },
                        effort: EffortName::Default,
                        required_capabilities: vec![],
                        pin: None,
                    },
                    input_tokens: 10,
                    output_headroom: 10,
                    unit_headroom: vec![],
                    expected_context_epoch: Some(0),
                    dispatch: Some(DispatchIdentity {
                        root_id: RootId(id(execution_root)),
                        execution_id: activation.id,
                        owner_epoch: activation.owner_epoch,
                        assignment_id: worker.then_some(assignment.id.clone()),
                        assignment_generation: assignment.generation,
                        turn_id: turn.turn_id,
                        implementation: false,
                    }),
                    cache_diagnostics: vec![],
                };
                if physical_child || manual_failed_close.is_some() {
                    use devworks_core::ports::usage::PriceResolver;
                    request.quote = read(&*f.store, |s| {
                        devworks_core::services::prices::CatalogPriceResolver
                            .quote(s, &request.query)
                    });
                    request.override_revision = 2;
                    assert_eq!(
                        request.quote.classification,
                        CostClassification::ConfiguredEstimate
                    );
                }
                let logical = RequestAdmission {
                    request_id: id("owned-request"),
                    purpose: Purpose::Conversation,
                    session_id: Some(assignment.session_id.clone()),
                    project_id: Some(ProjectId(id("project-a"))),
                    root_id: Some(assignment.work_id.clone()),
                    model_id: id("usage-model"),
                    connection_id: id("usage-connection"),
                    pin_id: None,
                    args_digest: "a".repeat(64),
                };
                let runtime = app.as_ref().map_or_else(
                    || {
                        UsageRuntime::with_process_host(
                            f.store.clone(),
                            f.service.clone(),
                            Some(selected_router),
                            Arc::new(devworks_adapters::process::NativeProcessHost),
                        )
                    },
                    |app| app.accounting().clone(),
                );
                let mut manual_task = None;
                let mut manual_before = None;
                let manual_release=Arc::new((Mutex::new(false),Condvar::new()));
                let _manual_cleanup=manual_failed_close.map(|_|Cleanup{runtime:runtime.clone(),gate:manual_release.clone()});
                if let Some(fail_close)=manual_failed_close {
                    let agent=f.store.info().host_actor;
                    f.tx(move|_,tx|tx.sessions().insert_session(&StoredSession{id:id("manual-feature-chat"),conversation_id:id("manual-feature-conversation"),agent_id:agent,project_id:Some(ProjectId(id("project-a"))),root_id:Some(RootId(id("root-f"))),title:"Human feature chat".into(),context_epoch:0,revision:1,created_by:ActorId(id("user-a")),created_at_ms:T0,closed_at_ms:None,closed_by:None,next_sequence:1,checkpoint_sequence:0}));
                    let manual_context=f.user.clone().with_lineage(Some(ProjectId(id("project-a"))),Some(RootId(id("root-f"))),None).with_session(id("manual-feature-chat"));
                    let mut manual_request=request.clone();manual_request.actor_id=manual_context.principal().clone();manual_request.role="Human".into();manual_request.request_id=id("manual-request");manual_request.admission_key=id("manual-admission");manual_request.dispatch=None;
                    let mut manual_logical=logical.clone();manual_logical.request_id=manual_request.request_id.clone();manual_logical.session_id=Some(id("manual-feature-chat"));
                    let (observed,ready)=tokio::sync::oneshot::channel();
                    let source=Box::new(ManualChatSource{first:true,observed:Some(observed),release:manual_release.clone(),fail_close});let host=runtime.clone();
                    let mut task=tokio::spawn(async move{host.produce(manual_context,manual_logical,ReportCategory::Assistant,manual_request,source).await});
                    ready.await.unwrap();
                    if fail_close{assert!(matches!((&mut task).await.unwrap(),Err(StorageError::Busy)));}else{manual_task=Some(task);}
                    let rows=read(&*f.store,|s|s.usage().inference_attempts(&UsageAttemptFilter{session_id:Some(id("manual-feature-chat")),..Default::default()},None,64));assert_eq!(rows.len(),1);let attempt=&rows[0];assert_eq!(attempt.feature_id,Some(id("root-f")));assert!(attempt.execution_root_id.is_none()&&attempt.execution_id.is_none());
                    let balance=read(&*f.store,|s|s.usage().attempt_balance(&attempt.attempt_id)).unwrap();assert_eq!(balance.priced_micro_usd,Some(10));assert_eq!(balance.reserved_micro_usd,Some(20));assert!(!balance.local_quiescent);
                    assert!(read(&*f.store,|s|s.runs().inference_quiescent(&RootId(id("root-f")))));
                    manual_before=Some(balance);
                }
                if panic_source {
                    assert!(
                        runtime
                            .produce(
                                context.clone(),
                                logical.clone(),
                                ReportCategory::Assistant,
                                request.clone(),
                                Box::new(PanickingSource)
                            )
                            .await
                            .is_err()
                    );
                    let rows = read(&*f.store, |s| {
                        s.usage()
                            .inference_attempts(&UsageAttemptFilter::default(), None, 64)
                    });
                    let balance = read(&*f.store, |s| {
                        s.usage().attempt_balance(&rows[0].attempt_id)
                    })
                    .unwrap();
                    assert!(!balance.local_quiescent && balance.unknown_tail);
                    assert!(
                        devworks_core::services::usage::accounting::summary(&balance)
                            .unwrap()
                            .custody_recovery_reason
                            .is_some()
                    );
                    let mut retry = request;
                    retry.request_id = id("same-turn-retry");
                    retry.admission_key = id("same-turn-retry-key");
                    let mut retry_logical = logical;
                    retry_logical.request_id = retry.request_id.clone();
                    assert!(matches!(
                        runtime
                            .produce(
                                context,
                                retry_logical,
                                ReportCategory::Assistant,
                                retry,
                                Box::new(NeverOpened)
                            )
                            .await
                            .unwrap(),
                        DriveOutcome::Refused(
                            devworks_core::services::usage::accounting::Admission::Fenced(_)
                        )
                    ));
                    assert_eq!(
                        read(&*f.store, |s| s
                            .usage()
                            .feature_usage_totals(&id(execution_root)))
                        .attempts,
                        1
                    );
                    runtime.stop();
                    runtime.drain().await;
                    continue;
                }
                let mut missing = request.clone();
                missing.dispatch = None;
                missing.request_id = id("missing-custody");
                missing.admission_key = id("missing-custody-key");
                let mut missing_logical = logical.clone();
                missing_logical.request_id = missing.request_id.clone();
                assert!(matches!(
                    runtime
                        .produce(
                            context.clone(),
                            missing_logical,
                            ReportCategory::Assistant,
                            missing,
                            Box::new(NeverOpened)
                        )
                        .await,
                    Err(StorageError::ScopeDenied)
                ));
                assert!(
                    read(&*f.store, |s| s
                        .router()
                        .route_request(&id("missing-custody")))
                    .is_none()
                );
                let (started, start) = tokio::sync::oneshot::channel();
                let (closing, close) = tokio::sync::oneshot::channel();
                let gate = Arc::new((Mutex::new(false), Condvar::new()));
                let _cleanup = Cleanup {
                    runtime: runtime.clone(),
                    gate: gate.clone(),
                };
                let source = Box::new(Source {
                    started: Some(started),
                    closing: Some(closing),
                    close_gate: gate.clone(),
                });
                let host = runtime.clone();
                let produce = tokio::spawn(async move {
                    host.produce(context, logical, ReportCategory::Assistant, request, source)
                        .await
                });
                if start.await.is_err(){panic!("valid independent coordinator was not admitted: {:?}",produce.await.unwrap());}
                if let Some(before)=&manual_before{assert_eq!(read(&*f.store,|s|s.usage().attempt_balance(&before.attempt_id)).as_ref(),Some(before),"managed admission must not retire or reprice manual custody");}
                if let Some(signal) = std::env::var_os("DW21_CAPTURE_OWNER") {
                    let attempts = read(&*f.store, |s| {
                        s.usage()
                            .inference_attempts(&UsageAttemptFilter::default(), None, 64)
                    });
                    std::fs::write(
                        signal,
                        serde_json::to_vec(&(f.h.root.clone(), attempts[0].attempt_id.clone()))
                            .unwrap(),
                    )
                    .unwrap();
                    std::future::pending::<()>().await;
                }
                assert!(!read(&*f.store, |s| s
                    .runs()
                    .inference_quiescent(&RootId(id(execution_root)))));
                if let Some(app) = &app {
                    let began = std::time::Instant::now();
                    let report = app.shutdown().await;
                    assert!(
                        began.elapsed() < Duration::from_secs(2),
                        "shutdown awaited foreign close"
                    );
                    close.await.unwrap();
                    assert_eq!(report.unclosed_inference_attempts.len(), 1);
                    let attempt_id = report.unclosed_inference_attempts[0].clone();
                    assert!(matches!(report.into_result(), Err(StorageError::Busy)));
                    let before = read(&*f.store, |s| {
                        Ok((
                            s.usage().inference_attempt(&attempt_id)?,
                            s.usage().attempt_balance(&attempt_id)?,
                        ))
                    });
                    assert!(!before.1.as_ref().unwrap().local_quiescent);
                    let diagnostics = read(&*f.store, |s| {
                        s.diagnostics().diagnostics(
                            &DiagnosticFilter {
                                component: Some("router.usage".into()),
                                operation: Some("runtime.shutdown".into()),
                                target_id: Some(attempt_id.clone()),
                                ..Default::default()
                            },
                            64,
                        )
                    });
                    assert!(!diagnostics.is_empty());
                    assert_eq!(refused(&f.resume(&f.user, execution_root)).0, "Conflict");
                    let owner = devworks_core::services::usage::accounting::snapshot(
                        before.0.as_ref().unwrap(),
                    )
                    .unwrap()
                    .host_process
                    .unwrap();
                    assert_eq!(owner.pid, std::process::id());
                    assert!(!owner.birth.is_empty());
                    {
                        let (lock, changed) = &*gate;
                        *lock.lock().unwrap() = true;
                        changed.notify_all();
                    }
                    assert!(matches!(
                        produce.await.unwrap(),
                        Err(StorageError::Cancelled)
                    ));
                    let after = read(&*f.store, |s| {
                        Ok((
                            s.usage().inference_attempt(&attempt_id)?,
                            s.usage().attempt_balance(&attempt_id)?,
                        ))
                    });
                    assert_eq!(
                        before, after,
                        "fenced callback wrote after shutdown returned"
                    );
                    assert!(!read(&*f.store, |s| s
                        .runs()
                        .inference_quiescent(&RootId(id(execution_root)))));
                    continue;
                }
                if membership {
                    assert!(matches!(
                        decision(&f.stop(&f.user, execution_root)),
                        DomainDecision::Accepted { .. }
                    ));
                    let x = read(&*f.store, |s| {
                        s.runs().live_activation(&RootId(id(execution_root)))
                    })
                    .unwrap();
                    f.tx(move |service, tx| {
                        service.finish_activation(
                            tx,
                            &x.id,
                            x.owner_epoch,
                            ActivationOutcome::Cancelled,
                            None,
                        )
                    })
                    .unwrap();
                    applied(&f.set_feature(execution_root, Some("root-g")));
                    f.tx(|service, tx| {
                        devworks_core::services::work::membership::reconcile_project(
                            tx,
                            service,
                            &ProjectId(id("project-a")),
                        )
                    });
                    assert!(!read(&*f.store, |s| s
                        .runs()
                        .assignment_handoff_quiescent(&assignment.id)));
                    assert_eq!(
                        read(&*f.store, |s| s.runs().assignment(&assignment.id))
                            .unwrap()
                            .root_id,
                        RootId(id(execution_root))
                    );
                } else {
                    let stopped = f.stop(&f.user, execution_root);
                    assert!(
                        matches!(decision(&stopped), DomainDecision::Accepted { .. }),
                        "{stopped:?}"
                    );
                }
                runtime.tick().await.unwrap();
                close.await.unwrap();
                // Source has observed cancellation, but its actual local close is still blocked.
                assert!(!read(&*f.store, |s| s
                    .runs()
                    .inference_quiescent(&RootId(id(execution_root)))));
                {
                    let (lock, changed) = &*gate;
                    *lock.lock().unwrap() = true;
                    changed.notify_all();
                }
                let DriveOutcome::Settled(balance) = produce.await.unwrap().unwrap() else {
                    panic!("source did not settle")
                };
                assert!(balance.local_quiescent && balance.unknown_tail);
                assert!(read(&*f.store, |s| s
                    .runs()
                    .inference_quiescent(&RootId(id(execution_root)))));
                let attempt = read(&*f.store, |s| {
                    s.usage().inference_attempt(&balance.attempt_id)
                })
                .unwrap();
                assert_eq!(attempt.assignment_id.is_some(), worker);
                assert_eq!(attempt.execution_root_id, Some(RootId(id(execution_root))));
                assert_eq!(attempt.job_run_id, worker.then(|| id("feature-job-run")));
                assert_eq!(
                    attempt.feature_id,
                    (!membership).then(|| id(execution_root))
                );
                let totals = read(&*f.store, |s| {
                    s.usage().feature_usage_totals(&id(execution_root))
                });
                assert_eq!(totals.attempts, if membership { 0 } else { 1 } + i64::from(manual_failed_close.is_some()));
                if let Some(destination) = destination {
                    f.tx(|service, tx| {
                        devworks_core::services::work::membership::reconcile_project(
                            tx,
                            service,
                            &ProjectId(id("project-a")),
                        )
                    });
                    let moved = read(&*f.store, |s| s.runs().assignment(&assignment.id)).unwrap();
                    assert_eq!(moved.root_id, RootId(id("root-g")));
                    assert_eq!(moved.parent_id, Some(destination.id));
                    assert_eq!(moved.session_id, assignment.session_id);
                    assert!(moved.generation > assignment.generation);
                }
                runtime.drain().await;
                if let Some(task)=manual_task{assert!(matches!(task.await.unwrap().unwrap(),DriveOutcome::Settled(_)));}
                if manual_failed_close==Some(true){let before=manual_before.unwrap();assert_eq!(read(&*f.store,|s|s.usage().attempt_balance(&before.attempt_id)),Some(before),"failed manual close retains its own expense/reservation without claiming execution custody");}
            }
        })
        .await;
    });
}

#[test]
fn primary_job_custody_is_rechecked_before_dispatch_and_while_the_source_waits() {
    runtime().block_on(within("primary job inference custody", async {
        for started in [false, true] {
            let f = Fx::new();
            let router = router(&f);
            let run_id = job_run(&f, true);
            // The linked feature's cap belongs to feature work, never the job's own scan.
            f.tx(|_, tx| {
                let work = tx.work().work(&id("root-f"))?.unwrap();
                devworks_core::services::usage::budget::configure(
                    tx,
                    &work,
                    &ActorId(id("user-a")),
                    &devworks_core::protocol::records::FeatureBudgetInput::Limit {
                        micro_usd: devworks_core::protocol::Counter::new(0).unwrap(),
                    },
                )
                .map(|_| ())
            });
            let info = f.store.info();
            let ctx = devworks_core::context::HostIngress::new(&*f.store)
                .context(
                    info.host_actor,
                    PrincipalKind::Host,
                    id("job-inference"),
                    id("native-job"),
                    devworks_core::protocol::Counter::new(info.restore_epoch).unwrap(),
                    Revision::new(1).unwrap(),
                )
                .with_lineage(Some(ProjectId(id("project-a"))), None, None);
            let request = AttemptRequest {
                admission_key: id("job-admission"),
                request_id: id("job-request"),
                actor_id: ctx.principal().clone(),
                work_id: None,
                job_run_id: Some(run_id.clone()),
                role: "Host".into(),
                query: PriceQuery {
                    catalog_model_id: id("usage-model"),
                    provider_model_id: Some("native-metadata-source".into()),
                    connection_id: id("usage-connection"),
                    endpoint_profile: "CompatibleChat".into(),
                    access_mode: "host-derived".into(),
                    service_tier: None,
                    input_tokens: Some(10),
                    cache_write_ttl_ms: None,
                    processing_region: None,
                    required_modalities: vec!["Text".into()],
                },
                quote: PriceQuote {
                    classification: CostClassification::LocalFree,
                    connection_revision: Some(1),
                    snapshot_id: None,
                    price_revision: None,
                    override_revision: None,
                    currency: "USD".into(),
                    rates: None,
                    tariffs: vec![],
                    untracked_reason: None,
                },
                price_generation: 1,
                override_revision: 1,
                pricing: PricingContext {
                    input_semantics: InputSemantics::Inclusive,
                    cache_read_possible: false,
                    cache_write_possible: false,
                    cache_write_ttl_ms: None,
                },
                selection: AttemptSelection::Conversation {
                    intent: RouteIntent::Model {
                        model_id: id("usage-model"),
                    },
                    effort: EffortName::Default,
                    required_capabilities: vec![],
                    pin: None,
                },
                input_tokens: 10,
                output_headroom: 10,
                unit_headroom: vec![],
                expected_context_epoch: None,
                dispatch: None,
                cache_diagnostics: vec![],
            };
            let logical = RequestAdmission {
                request_id: id("job-request"),
                purpose: Purpose::Conversation,
                session_id: None,
                project_id: Some(ProjectId(id("project-a"))),
                root_id: None,
                model_id: id("usage-model"),
                connection_id: id("usage-connection"),
                pin_id: None,
                args_digest: "a".repeat(64),
            };
            let cancel = |f: &Fx| {
                f.tx(|_, tx| {
                    let mut run = tx.jobs().job_run(&id("feature-job-run"))?.unwrap();
                    run.cancel_requested = true;
                    tx.jobs().update_run(&run, run.revision).map(|_| ())
                });
            };
            if !started {
                let host = devworks_core::services::usage::host::AttemptHost::new(
                    f.store.clone(),
                    f.service.clone(),
                    router,
                );
                let devworks_core::services::usage::accounting::Admission::Admitted {
                    attempt, ..
                } = host
                    .admit(ctx, logical, ReportCategory::Other, request)
                    .unwrap()
                else {
                    panic!("job not admitted")
                };
                assert_eq!(
                    devworks_core::services::usage::accounting::snapshot(&attempt)
                        .unwrap()
                        .job_run_revision,
                    Some(1)
                );
                cancel(&f);
                assert!(matches!(
                    host.drive(
                        attempt.attempt_id.clone(),
                        Box::new(NeverOpened),
                        Cancellation::new()
                    )
                    .unwrap(),
                    DriveOutcome::Refused(_)
                ));
                let balance = read(&*f.store, |s| {
                    s.usage().attempt_balance(&attempt.attempt_id)
                })
                .unwrap();
                assert!(balance.local_quiescent);
                assert_eq!(attempt.feature_id, None);
                continue;
            }
            let runtime = UsageRuntime::new(f.store.clone(), f.service.clone(), Some(router));
            let (started, start) = tokio::sync::oneshot::channel();
            let (closing, close) = tokio::sync::oneshot::channel();
            let gate = Arc::new((Mutex::new(false), Condvar::new()));
            let _cleanup = Cleanup {
                runtime: runtime.clone(),
                gate: gate.clone(),
            };
            let source = Box::new(Source {
                started: Some(started),
                closing: Some(closing),
                close_gate: gate.clone(),
            });
            let worker = runtime.clone();
            let produce = tokio::spawn(async move {
                worker
                    .produce(ctx, logical, ReportCategory::Other, request, source)
                    .await
            });
            start.await.unwrap();
            cancel(&f);
            runtime.tick().await.unwrap();
            close.await.unwrap();
            let attempts = read(&*f.store, |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 64)
            });
            assert_eq!(attempts.len(), 1);
            let attempt = &attempts[0];
            assert_eq!(attempt.job_run_id, Some(run_id));
            assert_eq!(attempt.feature_id, None);
            assert_eq!(attempt.execution_root_id, None);
            assert!(
                !read(&*f.store, |s| s
                    .usage()
                    .attempt_balance(&attempt.attempt_id))
                .unwrap()
                .local_quiescent
            );
            {
                let (lock, changed) = &*gate;
                *lock.lock().unwrap() = true;
                changed.notify_all();
            }
            let DriveOutcome::Settled(balance) = produce.await.unwrap().unwrap() else {
                panic!("job source not settled")
            };
            assert!(balance.local_quiescent && balance.unknown_tail);
            runtime.drain().await;
        }
    }));
}

/// The old owner is a real process running the same managed metadata producer. A coherent
/// SQLite clone represents a reopened/restored workspace while that owner is still alive.
#[test]
fn recovery_retains_live_native_owner_and_retires_only_after_verified_process_exit() {
    runtime().block_on(within("physical inference owner recovery", async {
        use devworks_adapters::process::NativeProcessHost;
        use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
        use devworks_core::ports::{IdKind, IdSource};
        struct FreshIds(AtomicUsize);
        impl IdSource for FreshIds {
            fn next_id(&self, kind: IdKind) -> Id {
                id(&format!("recovery-{}-{kind:?}-{}", std::process::id(), self.0.fetch_add(1, Ordering::SeqCst)))
            }
        }
        let h = Harness::new();
        std::fs::create_dir_all(&h.root).unwrap();
        let signal = h.root.join("owner-ready.json");
        let mut spec = ProcessSpec::new(&[
            std::env::current_exe().unwrap().to_string_lossy().into_owned(),
            "--exact".into(),
            "usage_tests::actual_coordinator_and_worker_transports_hold_quiescence_until_owned_close".into(),
            "--nocapture".into(),
        ]).unwrap();
        spec.env.push(("DW21_CAPTURE_OWNER".into(), signal.to_string_lossy().into_owned()));
        let native = Arc::new(NativeProcessHost);
        let mut child = native.spawn(&spec).unwrap();
        let (original, attempt_id): (std::path::PathBuf, Id) = loop {
            if let Ok(bytes) = std::fs::read(&signal)
                && let Ok(ready) = serde_json::from_slice(&bytes) { break ready; }
            assert!(!child.poll().unwrap().empty, "metadata owner exited before source start");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let original_cleanup = original.clone();
        struct ChildFiles(std::path::PathBuf);
        impl Drop for ChildFiles { fn drop(&mut self) { let _=std::fs::remove_dir_all(&self.0); } }
        let _files = ChildFiles(original_cleanup);
        let connection = rusqlite::Connection::open(original.join("workspace.sqlite")).unwrap();
        connection.execute("VACUUM INTO ?1", [h.root.join("workspace.sqlite").to_string_lossy().as_ref()]).unwrap();
        drop(connection);
        let ids = Arc::new(FreshIds(AtomicUsize::new(0)));
        let store: Arc<dyn ApplicationStore> = Arc::new(SqliteStore::open(SqliteConfig::new(
            h.root.clone(), h.clock(), ids.clone(), ActorId(id("host")),
        )).unwrap());
        let service = Arc::new(ControlService::with_scope(ids, Arc::new(ScriptedScope::new())));
        let recovery_service = service.clone();
        write(store.as_ref(), move |tx| recovery_service.recover_boot(tx, &NoLifecycleHooks)).unwrap();
        let before = read(store.as_ref(), |s| s.usage().attempt_balance(&attempt_id)).unwrap();
        assert_eq!(before.reserved_micro_usd, Some(30));
        let attempt = read(store.as_ref(), |s| s.usage().inference_attempt(&attempt_id)).unwrap();
        let admitted = devworks_core::services::usage::accounting::snapshot(&attempt).unwrap();
        let owner = admitted.host_process.unwrap();
        assert_eq!(owner.pid, child.identity().pid);
        assert_eq!(owner.birth, child.identity().birth);
        let identity = NativeIdentity { pid: owner.pid, birth: owner.birth, group: None };
        assert_eq!(native.probe(&identity), Probe::Alive);
        let host = UsageRuntime::with_process_host(store.clone(), service.clone(), None, native.clone());
        let pass = host.tick().await.unwrap();
        assert!(!pass.recovery_complete);
        let live = read(store.as_ref(), |s| s.usage().attempt_balance(&attempt_id)).unwrap();
        assert!(!live.local_quiescent);
        assert_eq!(live.reserved_micro_usd, before.reserved_micro_usd);
        assert!(!read(store.as_ref(), |s| s.runs().inference_quiescent(&RootId(id("root-f")))));
        let holds = read(store.as_ref(), |s| s.runs().blockers(&BlockerFilter::Root(RootId(id("root-f"))), None, 64));
        assert!(holds.iter().any(|b| b.state == BlockerState::Active && b.cause == HoldCause::RestartContinue));
        let kill_result = child.kill();
        // macOS may report an exiting group signal as EPERM after its root was killed.
        // Only subsequent physical observations authorize retirement.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if child.poll().unwrap().empty { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap_or_else(|_| panic!("native owner did not exit: {kill_result:?}"));
        assert_eq!(native.probe(&identity), Probe::Gone);
        assert!(host.tick().await.unwrap().recovery_complete);
        let retired = read(store.as_ref(), |s| s.usage().attempt_balance(&attempt_id)).unwrap();
        assert!(retired.local_quiescent && retired.unknown_tail);
        assert_eq!(retired.reserved_micro_usd, None);
        assert!(read(store.as_ref(), |s| s.runs().inference_quiescent(&RootId(id("root-f")))));
        assert_eq!(read(store.as_ref(), |s| s.runs().blockers(&BlockerFilter::Root(RootId(id("root-f"))), None, 64)), holds);
        host.stop(); host.drain().await;
    }));
}

#[test]
fn settled_unknown_and_cancelled_producer_replay_never_reopens_the_same_admission() {
    runtime().block_on(async {
        struct TerminalSource {
            opened: Arc<AtomicUsize>,
            terminal: Option<devworks_core::services::usage::accounting::AttemptEnd>,
        }
        impl OwnedInferenceSource for TerminalSource {
            fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
                self.opened.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn next(&mut self, _: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
                Ok(self.terminal.take().map(InferenceFrame::Terminal))
            }
            fn close(&mut self) -> Result<(), StorageError> {
                Ok(())
            }
        }
        use devworks_core::services::usage::accounting::AttemptEnd;
        for end in [
            Some(AttemptEnd::Completed),
            Some(AttemptEnd::UnknownTail),
            Some(AttemptEnd::Cancelled),
            None,
        ] {
            let f = Fx::new();
            let selected_router = router(&f);
            let info = f.store.info();
            let context = devworks_core::context::HostIngress::new(f.store.as_ref()).context(
                info.host_actor,
                PrincipalKind::Host,
                id("replay-invocation"),
                id("native-replay"),
                devworks_core::protocol::Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            );
            let logical = RequestAdmission {
                request_id: id("replay-request"),
                purpose: Purpose::Conversation,
                session_id: None,
                project_id: None,
                root_id: None,
                model_id: id("usage-model"),
                connection_id: id("usage-connection"),
                pin_id: None,
                args_digest: "a".repeat(64),
            };
            let request = AttemptRequest {
                admission_key: id("replay-admission"),
                request_id: logical.request_id.clone(),
                actor_id: context.principal().clone(),
                work_id: None,
                job_run_id: None,
                role: "Host".into(),
                query: PriceQuery {
                    catalog_model_id: id("usage-model"),
                    provider_model_id: Some("native-metadata-source".into()),
                    connection_id: id("usage-connection"),
                    endpoint_profile: "CompatibleChat".into(),
                    access_mode: "host-derived".into(),
                    service_tier: None,
                    input_tokens: Some(10),
                    cache_write_ttl_ms: None,
                    processing_region: None,
                    required_modalities: vec!["Text".into()],
                },
                quote: PriceQuote {
                    classification: CostClassification::LocalFree,
                    connection_revision: Some(1),
                    snapshot_id: None,
                    price_revision: None,
                    override_revision: None,
                    currency: "USD".into(),
                    rates: None,
                    tariffs: vec![],
                    untracked_reason: None,
                },
                price_generation: 1,
                override_revision: 1,
                pricing: PricingContext {
                    input_semantics: InputSemantics::Inclusive,
                    cache_read_possible: false,
                    cache_write_possible: false,
                    cache_write_ttl_ms: None,
                },
                selection: AttemptSelection::Conversation {
                    intent: RouteIntent::Model {
                        model_id: id("usage-model"),
                    },
                    effort: EffortName::Default,
                    required_capabilities: vec![],
                    pin: None,
                },
                input_tokens: 10,
                output_headroom: 10,
                unit_headroom: vec![],
                expected_context_epoch: None,
                dispatch: None,
                cache_diagnostics: vec![],
            };
            let owned = UsageRuntime::new(
                f.store.clone(),
                f.service.clone(),
                Some(selected_router.clone()),
            );
            let opened = Arc::new(AtomicUsize::new(0));
            if let Some(end) = end {
                let result = owned
                    .produce(
                        context.clone(),
                        logical.clone(),
                        ReportCategory::Other,
                        request.clone(),
                        Box::new(TerminalSource {
                            opened: opened.clone(),
                            terminal: Some(end),
                        }),
                    )
                    .await
                    .unwrap();
                assert!(matches!(result, DriveOutcome::Settled(_)));
            } else {
                let host = devworks_core::services::usage::host::AttemptHost::new(
                    f.store.clone(),
                    f.service.clone(),
                    selected_router,
                );
                let devworks_core::services::usage::accounting::Admission::Admitted {
                    attempt, ..
                } = host
                    .admit(
                        context.clone(),
                        logical.clone(),
                        ReportCategory::Other,
                        request.clone(),
                    )
                    .unwrap()
                else {
                    panic!("not admitted")
                };
                let cancel = Cancellation::new();
                cancel.cancel();
                assert!(matches!(
                    host.drive(
                        attempt.attempt_id,
                        Box::new(TerminalSource {
                            opened: opened.clone(),
                            terminal: None
                        }),
                        cancel
                    )
                    .unwrap(),
                    DriveOutcome::CancelledBeforeDispatch
                ));
            }
            let before = read(f.store.as_ref(), |s| {
                let attempts =
                    s.usage()
                        .inference_attempts(&UsageAttemptFilter::default(), None, 64)?;
                assert_eq!(attempts.len(), 1);
                Ok((
                    attempts[0].clone(),
                    s.usage().attempt_balance(&attempts[0].attempt_id)?.unwrap(),
                ))
            });
            let result = owned
                .produce(
                    context,
                    logical,
                    ReportCategory::Other,
                    request,
                    Box::new(TerminalSource {
                        opened: opened.clone(),
                        terminal: Some(AttemptEnd::Completed),
                    }),
                )
                .await
                .unwrap();
            assert!(
                matches!(result, DriveOutcome::AlreadyDispatched),
                "{result:?}"
            );
            assert_eq!(opened.load(Ordering::SeqCst), usize::from(end.is_some()));
            let after = read(f.store.as_ref(), |s| {
                let attempts =
                    s.usage()
                        .inference_attempts(&UsageAttemptFilter::default(), None, 64)?;
                assert_eq!(attempts.len(), 1);
                Ok((
                    attempts[0].clone(),
                    s.usage().attempt_balance(&attempts[0].attempt_id)?.unwrap(),
                ))
            });
            assert_eq!(
                before, after,
                "same admission must preserve canonical attempt and balance"
            );
        }
    });
}
