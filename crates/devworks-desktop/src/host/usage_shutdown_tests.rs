use super::*;
use devworks_core::{
    context::{HostIngress, PrincipalKind},
    ports::{storage::*, usage::*},
    protocol::{
        Counter, Id, Revision,
        records::{EffortName, RouteIntent},
    },
    services::{
        router::{
            Purpose, RequestAdmission,
            accounting::{AttemptSelection, ReportCategory},
        },
        usage::{
            accounting::AttemptRequest,
            host::{InferenceFrame, OwnedInferenceSource},
            meter::InputSemantics,
            pricing::PricingContext,
        },
    },
};
use std::sync::{Condvar, Mutex};
fn id(v: &str) -> Id {
    Id::new(v).unwrap()
}
struct Gated {
    start: bool,
    ready: Option<mpsc::Sender<()>>,
    entered: Option<mpsc::Sender<()>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    finished: Option<mpsc::Sender<()>>,
}
impl Drop for Gated {
    fn drop(&mut self) {
        if let Some(done) = self.finished.take() {
            let _ = done.send(());
        }
    }
}
impl Gated {
    fn wait(&mut self) {
        let _ = self.entered.take().unwrap().send(());
        let mut open = self.gate.0.lock().unwrap();
        while !*open {
            open = self.gate.1.wait(open).unwrap();
        }
    }
}
impl OwnedInferenceSource for Gated {
    fn start(&mut self, _: &StoredInferenceAttempt) -> Result<(), StorageError> {
        let _ = self.ready.take().unwrap().send(());
        if self.start {
            self.wait();
        }
        Ok(())
    }
    fn next(&mut self, cancel: &Cancellation) -> Result<Option<InferenceFrame>, StorageError> {
        let mut open = self.gate.0.lock().unwrap();
        while !cancel.is_cancelled() {
            open = self
                .gate
                .1
                .wait_timeout(open, Duration::from_millis(2))
                .unwrap()
                .0;
        }
        Ok(None)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        if !self.start {
            self.wait();
        }
        Ok(())
    }
}
#[test]
fn actual_engine_and_tokio_drop_finish_while_owned_start_or_close_is_stalled() {
    for start in [true, false] {
        let root =
            std::env::temp_dir().join(format!("dw21-engine-drop-{}-{start}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut config = DesktopConfig::new(&root);
        config.shutdown_bound = Duration::from_millis(30);
        config.connection_tick = None;
        config.indexing.discover_at_boot = false;
        config.secrets = Some(Arc::new(
            devworks_core::ports::secrets::SessionSecrets::new(),
        ));
        config.http = Some(Arc::new(devworks_core::ports::http::NoNetwork));
        let engine = Engine::start(config).unwrap();
        let app = engine.host.runtime.clone();
        let store = app.store().clone();
        run_write(store.as_ref(), WriteOptions::default(), |tx| {
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
                    created_at_ms: 1,
                    updated_at_ms: 1,
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
                    updated_at_ms: 1,
                },
                None,
            )
        })
        .unwrap();
        let info = store.info();
        let context = HostIngress::new(store.as_ref()).context(
            info.host_actor,
            PrincipalKind::Host,
            id("drop-invocation"),
            id("drop-caller"),
            Counter::new(info.restore_epoch).unwrap(),
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

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (entered, signal) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let usage = app.accounting().clone();
        let (finished, retired) = mpsc::channel();
        let source_gate = gate.clone();
        let job = engine.host.async_runtime.spawn(async move {
            usage
                .produce(
                    context,
                    logical,
                    ReportCategory::Other,
                    request,
                    Box::new(Gated {
                        start,
                        ready: Some(ready),
                        entered: Some(entered),
                        gate: source_gate,
                        finished: Some(finished),
                    }),
                )
                .await
        });
        started.recv_timeout(Duration::from_secs(10)).unwrap();
        if start {
            signal.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let release = gate.clone();
        let (closed, done) = mpsc::channel();
        std::thread::spawn(move || {
            drop(engine);
            let _ = closed.send(());
        });
        if !start {
            signal.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        let result = done.recv_timeout(Duration::from_secs(2));
        let before = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
            Ok(s.stamp().committed_sequence)
        })
        .unwrap();
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        assert!(
            result.is_ok(),
            "Engine Drop must finish before external source release; {result:?}"
        );
        drop(job);
        retired.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            read_snapshot(store.as_ref(), &ReadOptions::default(), |s| Ok(s
                .stamp()
                .committed_sequence))
            .unwrap(),
            before,
            "late source cannot publish after runtime close"
        );
        drop(app);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
