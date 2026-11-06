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
                    job_run_id: None,
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
                runtime.drain().await;
            }
        })
        .await;
    });
}
