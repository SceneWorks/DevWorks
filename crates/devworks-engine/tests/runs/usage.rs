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
        accounting::{AttemptRequest, DispatchIdentity},
        host::{DriveOutcome, InferenceFrame, OwnedInferenceSource},
        meter::InputSemantics,
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

#[test]
fn actual_coordinator_and_worker_transports_hold_quiescence_until_owned_close() {
    runtime().block_on(async {
        within("managed inference closure", async {
            for worker in [false, true] {
                let f = Fx::new();
                assert!(matches!(
                    decision(&f.start(&f.user, "root-f", 1, 1)),
                    DomainDecision::Applied { .. }
                ));
                let activation = f.launch("usage-supervisor");
                let coordinator = f.assign("root-f", "root-f", None, None);
                let assignment = if worker {
                    f.assign("root-f", "task-f1", Some(&coordinator.id), None)
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
                        Some(RootId(id("root-f"))),
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
                let request = AttemptRequest {
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
                        root_id: RootId(id("root-f")),
                        execution_id: activation.id,
                        owner_epoch: activation.owner_epoch,
                        assignment_id: worker.then_some(assignment.id.clone()),
                        assignment_generation: assignment.generation,
                        turn_id: turn.turn_id,
                        implementation: false,
                    }),
                    cache_diagnostics: vec![],
                };
                let logical = RequestAdmission {
                    request_id: id("owned-request"),
                    purpose: Purpose::Conversation,
                    session_id: Some(assignment.session_id),
                    project_id: Some(ProjectId(id("project-a"))),
                    root_id: Some(assignment.work_id),
                    model_id: id("usage-model"),
                    connection_id: id("usage-connection"),
                    pin_id: None,
                    args_digest: "a".repeat(64),
                };
                let runtime =
                    UsageRuntime::new(f.store.clone(), f.service.clone(), Some(router(&f)));
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
                start.await.unwrap();
                assert!(!read(&*f.store, |s| s
                    .runs()
                    .inference_quiescent(&RootId(id("root-f")))));
                let stopped = f.stop(&f.user, "root-f");
                assert!(
                    matches!(decision(&stopped), DomainDecision::Accepted { .. }),
                    "{stopped:?}"
                );
                runtime.tick().await.unwrap();
                close.await.unwrap();
                // Source has observed cancellation, but its actual local close is still blocked.
                assert!(!read(&*f.store, |s| s
                    .runs()
                    .inference_quiescent(&RootId(id("root-f")))));
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
                    .inference_quiescent(&RootId(id("root-f")))));
                let attempt = read(&*f.store, |s| {
                    s.usage().inference_attempt(&balance.attempt_id)
                })
                .unwrap();
                assert_eq!(attempt.assignment_id.is_some(), worker);
                assert_eq!(attempt.execution_root_id, Some(RootId(id("root-f"))));
                assert_eq!(attempt.job_run_id, worker.then(|| id("feature-job-run")));
                assert_eq!(attempt.feature_id, Some(id("root-f")));
                let totals = read(&*f.store, |s| s.usage().feature_usage_totals(&id("root-f")));
                assert_eq!(totals.attempts, 1);
                runtime.drain().await;
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
