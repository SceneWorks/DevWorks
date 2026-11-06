//! Real gateway, SQLite, journal, durable delivery and host price maintenance. Scripted public
//! sources exercise publication/failure/races without network or any inference execution.
mod common;
use common::*;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::*;
use devworks_core::ports::usage::*;
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::OperationId;
use devworks_core::protocol::results::{DomainCommandReply, DomainDecision};
use devworks_core::services::{
    UseCases,
    prices::{PriceService, refresh},
};
use devworks_engine::prices::PriceSupervisor;
use devworks_engine::{Dispatcher, DispatcherConfig};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

struct Fx {
    h: Harness,
    store: Arc<dyn ApplicationStore>,
    service: Arc<PriceService>,
    cases: UseCases,
    user: InvocationContext,
    agent: InvocationContext,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = h.open();
        let user = principal(&*store, "user", PrincipalKind::Human);
        let agent = principal(&*store, "agent", PrincipalKind::Agent);
        seed(&*store, &h);
        let service = Arc::new(PriceService::new(h.ids.clone(), h.clock(), true));
        let cases = UseCases::new().with_prices(service.clone());
        Self {
            h,
            store,
            service,
            cases,
            user,
            agent,
        }
    }
    fn frame(&self, op: &str, schema: u32, mode: &str, mut body: Value) -> Vec<u8> {
        body["kind"] = json!(op);
        serde_json::to_vec(&if mode=="Query"{json!({"protocol":"1.0-draft.1","mode":mode,"schemaRevision":schema,"queryId":self.h.ids.next_id(IdKind::Link),"body":body})}else{json!({"protocol":"1.0-draft.1","mode":mode,"schemaRevision":schema,"requestKey":self.h.ids.next_id(IdKind::Link),"body":body})}).unwrap()
    }
    fn command(
        &self,
        ctx: &InvocationContext,
        op: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> DomainCommandReply {
        let authority = HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command_with(&gateway, ctx, op, raw, options)
            .unwrap()
    }
    fn query(&self, body: Value) -> Value {
        self.query_schema(body, 3)
    }
    fn query_schema(&self, body: Value, schema: u32) -> Value {
        let raw = self.frame("router.price_status", schema, "Query", body);
        let authority = HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        serde_json::to_value(
            self.cases
                .query(&gateway, &self.user, "router.price_status", &raw)
                .unwrap(),
        )
        .unwrap()
    }
    fn refresh(&self, providers: &[&str]) -> (Vec<u8>, OperationId) {
        let revision = read(&*self.store, |s| s.usage().price_revision());
        let raw = self.frame(
            "router.price_refresh",
            1,
            "Command",
            json!({"providerIds":providers,"expectedPriceRevision":revision.to_string()}),
        );
        let op = accepted(self.command(
            &self.user,
            "router.price_refresh",
            &raw,
            WriteOptions::default(),
        ));
        (raw, op)
    }
    fn dispatcher(&self, source: Arc<dyn PriceSource>) -> (Arc<PriceSupervisor>, Dispatcher) {
        let worker = PriceSupervisor::new(self.store.clone(), self.service.clone(), source);
        let dispatcher = Dispatcher::new(
            self.store.clone(),
            self.h.clock(),
            vec![worker.consumer()],
            DispatcherConfig::new(id("price-test")),
        )
        .unwrap();
        (worker, dispatcher)
    }
}
fn seed(store: &dyn ApplicationStore, h: &Harness) {
    let now = h.now();
    write(store, move |tx| {
        for (name, profile) in [
            ("openai-connection", "OpenAiResponses"),
            ("anthropic-connection", "AnthropicMessages"),
        ] {
            tx.router().put_connection(
                &StoredConnection {
                    id: id(name),
                    name: name.into(),
                    profile: profile.into(),
                    endpoint: if profile == "OpenAiResponses" {
                        "https://api.openai.com/v1"
                    } else {
                        "https://api.anthropic.com"
                    }
                    .into(),
                    enabled: true,
                    local_free: false,
                    open_router: None,
                    credential_ref: None,
                    credential_revision: 0,
                    account_revision: 1,
                    credential_epoch: None,
                    lifecycle: "Disconnected".into(),
                    state_reason: None,
                    account_digest: None,
                    access_expires_at_ms: None,
                    risk: None,
                    discovered: None,
                    discovered_at_ms: None,
                    healthy: None,
                    checked_at_ms: None,
                    revision: 1,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                None,
            )?;
        }
        for (model, profile, connection) in [
            ("priced-model", "OpenAiResponses", "openai-connection"),
            (
                "unpriced-model",
                "AnthropicMessages",
                "anthropic-connection",
            ),
        ] {
            tx.router().put_route_model(
                &StoredRouteModel {
                    id: id(model),
                    display_name: model.into(),
                    endpoint_profile: profile.into(),
                    upstream_model_id: model.into(),
                    enabled: true,
                    efforts: vec![],
                    image_input: false,
                    tools: false,
                    context_tokens: Some(100_000),
                    connection_id: Some(id(connection)),
                    position: 1000,
                    revision: 1,
                    updated_at_ms: now,
                },
                None,
            )?;
        }
        Ok(())
    })
    .unwrap();
}
fn accepted(reply: DomainCommandReply) -> OperationId {
    match reply {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn applied(reply: DomainCommandReply) -> Value {
    match reply {
        devworks_core::protocol::CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
fn rate(amount: &str) -> PublishedTariff {
    PublishedTariff {
        upstream_model_id: "priced-model".into(),
        service_tier: "Standard".into(),
        context_lower_exclusive: None,
        context_upper_inclusive: None,
        modality: "Text".into(),
        charge_kind: "Input".into(),
        cache_ttl_seconds: None,
        unit: "Token".into(),
        units: 1_000_000,
        amount_usd: amount.into(),
        region: None,
        endpoint_profile: None,
    }
}
fn catalog(provider: OfficialPriceProvider, now: i64, amount: &str) -> FetchedPriceCatalog {
    let input = rate(amount);
    let mut output = input.clone();
    output.charge_kind = "Output".into();
    FetchedPriceCatalog {
        provider,
        source_url: provider.source_url().into(),
        source_digest: "a".repeat(64),
        parser_version: "native-test-v1".into(),
        fetched_at_ms: now,
        verified_at_ms: now,
        effective_date: None,
        tariffs: vec![input, output],
    }
}
struct Source {
    outcomes: Mutex<VecDeque<Result<String, PriceFetchError>>>,
    calls: AtomicUsize,
    gate: Option<Arc<Gate>>,
}
impl Source {
    fn new(outcomes: Vec<Result<&str, PriceFetchError>>) -> Arc<Self> {
        Arc::new(Self {
            outcomes: Mutex::new(outcomes.into_iter().map(|v| v.map(str::to_owned)).collect()),
            calls: AtomicUsize::new(0),
            gate: None,
        })
    }
}
impl PriceSource for Source {
    fn fetch(
        &self,
        p: OfficialPriceProvider,
        now: i64,
    ) -> Result<FetchedPriceCatalog, PriceFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            g.started.notify_one();
            let mut open = g.open.lock().unwrap();
            while !*open {
                open = g.release.wait(open).unwrap();
            }
        }
        let amount = self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("1.25".into()))?;
        Ok(catalog(p, now, &amount))
    }
}
struct Gate {
    started: tokio::sync::Notify,
    open: Mutex<bool>,
    release: Condvar,
}
impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: tokio::sync::Notify::new(),
            open: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.release.notify_all();
    }
}
async fn deliver(dispatcher: &Dispatcher) {
    dispatcher.recover().await.unwrap();
    dispatcher.tick().await.unwrap();
    within("price delivery", dispatcher.idle()).await;
}

#[test]
fn manual_refresh_is_accepted_persisted_and_replayed_while_complete_snapshots_keep_last_good_on_failure()
 {
    runtime().block_on(async {
        let fx = Fx::new();
        let source = Source::new(vec![
            Ok("1.25"),
            Err(PriceFetchError::SourceChanged(
                "changed official markup".into(),
            )),
        ]);
        let (worker, dispatcher) = fx.dispatcher(source.clone());
        let (raw, op) = fx.refresh(&["openai"]);
        assert_eq!(source.calls.load(Ordering::SeqCst), 0, "no I/O in writer");
        let status = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
        assert!(status.refresh_pending);
        assert_eq!(status.refresh_operation_id, Some(op.clone()));
        assert_eq!(
            accepted(fx.command(
                &fx.user,
                "router.price_refresh",
                &raw,
                WriteOptions::default()
            )),
            op
        );
        deliver(&dispatcher).await;
        let first = read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap();
        assert_eq!(first.tariffs.len(), 2);
        assert_eq!(
            read(&*fx.store, |s| s.operations().operation(&op))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::ObservedComplete
        );
        let (_, failed) = fx.refresh(&["openai"]);
        deliver(&dispatcher).await;
        assert_eq!(
            read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap(),
            first
        );
        let status = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
        assert!(
            status
                .last_error
                .unwrap()
                .contains("changed official markup")
        );
        assert_eq!(status.refresh_operation_id, Some(failed.clone()));
        assert_eq!(
            read(&*fx.store, |s| s.operations().operation(&failed))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::Failed
        );
        worker.drain().await;
        dispatcher.stop();
    });
}

#[test]
fn global_refresh_cas_is_atomic_and_cancellation_never_records_intent() {
    let fx = Fx::new();
    let raw = fx.frame(
        "router.price_refresh",
        1,
        "Command",
        json!({"providerIds":["openai","missing-provider"],"expectedPriceRevision":"1"}),
    );
    let rejected = fx.command(
        &fx.user,
        "router.price_refresh",
        &raw,
        WriteOptions::default(),
    );
    assert!(
        !matches!(rejected,devworks_core::protocol::CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Accepted{..}))
    );
    assert!(read(&*fx.store, |s| s.usage().price_status("openai")).is_none());
    let raw = fx.frame(
        "router.price_refresh",
        1,
        "Command",
        json!({"providerIds":["openai"],"expectedPriceRevision":"1"}),
    );
    let cancellation = Cancellation::default();
    cancellation.cancel();
    fx.command(
        &fx.user,
        "router.price_refresh",
        &raw,
        WriteOptions {
            cancellation: Some(cancellation),
            ..WriteOptions::default()
        },
    );
    assert!(read(&*fx.store, |s| s.usage().price_status("openai")).is_none());
    let (_, _) = fx.refresh(&["openai"]);
    let stale = fx.frame(
        "router.price_refresh",
        1,
        "Command",
        json!({"providerIds":["anthropic"],"expectedPriceRevision":"1"}),
    );
    fx.command(
        &fx.user,
        "router.price_refresh",
        &stale,
        WriteOptions::default(),
    );
    assert!(read(&*fx.store, |s| s.usage().price_status("anthropic")).is_none());
}

#[test]
fn positive_baseline_override_requires_actual_human_preserves_dimensions_and_survives_refresh_until_clear()
 {
    runtime().block_on(async{let fx=Fx::new();let change=json!({"kind":"Set","tariffs":[{"upstreamModelId":"priced-model","serviceTier":"Priority","contextLowerExclusive":"1000","modality":"Audio","chargeKind":"Input","unit":"Second","units":"1","amountUsd":"0.025","region":"us","endpointProfile":"OpenAiResponses"}]});let raw=fx.frame("router.price_override",3,"Command",json!({"modelId":"priced-model","expectedRevision":"1","change":change}));let denied=fx.command(&fx.agent,"router.price_override",&raw,WriteOptions::default());assert!(!matches!(denied,devworks_core::protocol::CommandReply::Recorded{receipt,..}if matches!(receipt.decision,DomainDecision::Applied{..})));assert_eq!(read(&*fx.store,|s|s.usage().price_override_revision(&id("priced-model"))),1);let result=applied(fx.command(&fx.user,"router.price_override",&raw,WriteOptions::default()));assert_eq!(result["revision"],"2");let view=fx.query(json!({"modelIds":["priced-model"],"page":{"limit":50}}));assert_eq!(view["result"]["value"]["models"][0]["revision"],"2");let saved=read(&*fx.store,|s|s.usage().price_override(&id("priced-model"))).unwrap();assert_eq!(saved.tariffs[0].modality,"Audio");let(worker,dispatcher)=fx.dispatcher(Source::new(vec![]));fx.refresh(&["openai"]);deliver(&dispatcher).await;assert_eq!(read(&*fx.store,|s|s.usage().price_override(&id("priced-model"))).unwrap(),saved);let clear=fx.frame("router.price_override",3,"Command",json!({"modelId":"priced-model","expectedRevision":"2","change":{"kind":"Clear"}}));assert_eq!(applied(fx.command(&fx.user,"router.price_override",&clear,WriteOptions::default()))["revision"],"3");assert_eq!(read(&*fx.store,|s|s.usage().price_override(&id("priced-model"))).unwrap().mode,"Clear");let view=fx.query(json!({"modelIds":["priced-model"],"page":{"limit":50}}));assert_eq!(view["result"]["value"]["models"][0]["revision"],"3");worker.drain().await;dispatcher.stop();});
}

#[test]
fn price_catalog_pages_unpriced_models_dimensions_and_history_with_exact_query_bound_cursors() {
    runtime().block_on(async{
    let fx=Fx::new();let(worker,dispatcher)=fx.dispatcher(Source::new(vec![Ok("1.25"),Ok("2.50")]));fx.refresh(&["openai"]);deliver(&dispatcher).await;let first=read(&*fx.store,|s|s.usage().current_price_snapshot("openai")).unwrap();fx.refresh(&["openai"]);deliver(&dispatcher).await;
    let mut cursor:Option<Value>=None;let mut models=Vec::new();let mut snapshots=Vec::new();let mut rates=Vec::new();let mut first_cursor=None;
    for _ in 0..20{let mut body=json!({"modelIds":["priced-model","unpriced-model"],"includeHistory":true,"page":{"limit":2}});if let Some(cursor)=cursor.take(){body["page"]["cursor"]=cursor;}let reply=fx.query(body);assert_eq!(reply["kind"],"Result","{reply}");let view=&reply["result"]["value"];assert!(view["models"].as_array().unwrap().len()+view["items"].as_array().unwrap().len()+view["snapshots"].as_array().unwrap().len()+view["providers"].as_array().unwrap().len()<=2);models.extend(view["models"].as_array().unwrap().iter().cloned());snapshots.extend(view["snapshots"].as_array().unwrap().iter().cloned());rates.extend(view["items"].as_array().unwrap().iter().cloned());if !view["hasMore"].as_bool().unwrap(){break;}cursor=Some(view["nextCursor"].clone());if first_cursor.is_none(){first_cursor=cursor.clone();}}
    assert_eq!(models.len(),2);assert!(models.iter().any(|v|v["modelId"]=="unpriced-model"&&v["priceKind"]=="Untracked"));assert_eq!(snapshots.len(),2);assert_eq!(rates.len(),4);assert!(rates.iter().any(|v|v["tariff"]["amountUsd"]=="1.25"));assert!(rates.iter().any(|v|v["tariff"]["amountUsd"]=="2.50"));
    let wrong=fx.query(json!({"modelIds":["priced-model"],"includeHistory":true,"page":{"limit":2,"cursor":first_cursor.clone().unwrap()}}));assert_ne!(wrong["kind"],"Result","changed filter must not reuse cursor");
    let selected=fx.query(json!({"modelIds":["priced-model"],"snapshotIds":[first.id],"includeHistory":false,"page":{"limit":50}}));let selected=&selected["result"]["value"];assert_eq!(selected["snapshots"].as_array().unwrap().len(),1);assert!(selected["items"].as_array().unwrap().iter().all(|v|v["snapshotId"]==first.id.as_str()));
    let change=fx.frame("router.price_override",3,"Command",json!({"modelId":"priced-model","expectedRevision":"1","price":{"inputUsdPerMillion":"5","outputUsdPerMillion":"10"}}));applied(fx.command(&fx.user,"router.price_override",&change,WriteOptions::default()));let stale=fx.query(json!({"modelIds":["priced-model","unpriced-model"],"includeHistory":true,"page":{"limit":2,"cursor":first_cursor.unwrap()}}));assert_ne!(stale["kind"],"Result","price mutation expires earlier view");worker.drain().await;dispatcher.stop();
});
}

#[test]
fn changed_connection_binding_refuses_stale_publication_without_overwriting_last_good() {
    runtime().block_on(async {
        let fx = Fx::new();
        let (worker, dispatcher) = fx.dispatcher(Source::new(vec![]));
        fx.refresh(&["openai"]);
        deliver(&dispatcher).await;
        let first = read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap();
        worker.drain().await;
        dispatcher.stop();
        drop(dispatcher);
        drop(worker);
        let gate = Gate::new();
        let source = Arc::new(Source {
            outcomes: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let (worker, dispatcher) = fx.dispatcher(source);
        let (_, op) = fx.refresh(&["openai"]);
        dispatcher.recover().await.unwrap();
        dispatcher.tick().await.unwrap();
        within("price source GET admitted", gate.started.notified()).await;
        write(&*fx.store, |tx| {
            let mut connection = tx.router().connection(&id("openai-connection"))?.unwrap();
            let before = connection.revision;
            connection.revision += 1;
            connection.enabled = false;
            tx.router().put_connection(&connection, Some(before))
        })
        .unwrap();
        gate.release();
        within("stale publication settles", dispatcher.idle()).await;
        assert_eq!(
            read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap(),
            first
        );
        let status = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
        assert!(!status.refresh_pending);
        assert!(status.last_error.unwrap().contains("binding changed"));
        assert_eq!(status.refresh_operation_id, Some(op));
        worker.drain().await;
        dispatcher.stop();
    });
}

#[test]
fn shutdown_drains_inflight_get_without_publication_then_restart_reconciles_the_same_operation() {
    runtime().block_on(async {
        let fx = Fx::new();
        let gate = Gate::new();
        let source = Arc::new(Source {
            outcomes: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            gate: Some(gate.clone()),
        });
        let (worker, dispatcher) = fx.dispatcher(source);
        let (_, op) = fx.refresh(&["openai"]);
        dispatcher.recover().await.unwrap();
        dispatcher.tick().await.unwrap();
        within("price fetch in flight", gate.started.notified()).await;
        worker.stop();
        let draining = tokio::spawn({
            let worker = worker.clone();
            async move { worker.drain().await }
        });
        tokio::task::yield_now().await;
        assert!(!draining.is_finished());
        gate.release();
        draining.await.unwrap();
        within("price unknown settles", dispatcher.idle()).await;
        assert!(read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).is_none());
        assert_eq!(
            read(&*fx.store, |s| s.operations().operation(&op))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::OutcomeUnknown
        );
        assert_eq!(worker.tick().await.unwrap(), 0);
        dispatcher.stop();
        drop(dispatcher);
        drop(worker);
        let Fx {
            h,
            store,
            service,
            cases,
            user: _,
            agent: _,
        } = fx;
        drop(service);
        drop(cases);
        drop(store);
        let store = h.open();
        let service = Arc::new(PriceService::new(h.ids.clone(), h.clock(), true));
        let source = Source::new(vec![]);
        let worker = PriceSupervisor::new(store.clone(), service, source.clone());
        let dispatcher = Dispatcher::new(
            store.clone(),
            h.clock(),
            vec![worker.consumer()],
            DispatcherConfig::new(id("price-restart")),
        )
        .unwrap();
        dispatcher.recover().await.unwrap();
        worker.tick().await.unwrap();
        deliver(&dispatcher).await;
        assert_eq!(
            read(&*store, |s| s.operations().operation(&op))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::ObservedComplete
        );
        assert_eq!(
            read(&*store, |s| s.usage().price_status("openai"))
                .unwrap()
                .refresh_operation_id,
            Some(op)
        );
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            2,
            "one recovered OpenAI GET plus the configured Anthropic catch-up"
        );
        worker.drain().await;
        dispatcher.stop();
    });
}

#[test]
fn weekly_cadence_schedules_one_overdue_operation_and_keeps_overrides_and_history_immutable() {
    runtime().block_on(async {
        let fx = Fx::new();
        let source = Source::new(vec![]);
        let (worker, dispatcher) = fx.dispatcher(source.clone());
        assert_eq!(worker.tick().await.unwrap(), 2);
        let first = read(&*fx.store, |s| s.usage().price_status("openai"))
            .unwrap()
            .refresh_operation_id
            .unwrap();
        assert_eq!(worker.tick().await.unwrap(), 0);
        deliver(&dispatcher).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        let snapshot = read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap();
        assert_eq!(worker.tick().await.unwrap(), 0);
        fx.h.advance(refresh::WEEK_MS * 5);
        assert_eq!(worker.tick().await.unwrap(), 2);
        let next = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
        assert_ne!(next.refresh_operation_id, Some(first));
        assert_eq!(worker.tick().await.unwrap(), 0, "missed weeks never replay");
        deliver(&dispatcher).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            read(&*fx.store, |s| s.usage().price_snapshot(&snapshot.id)).unwrap(),
            snapshot
        );
        assert_eq!(
            read(&*fx.store, |s| s.usage().current_price_snapshot("openai"))
                .unwrap()
                .revision,
            2
        );
        worker.drain().await;
        dispatcher.stop();
    });
}

#[test]
fn source_filters_and_legacy_results_are_exact_or_truthfully_unrepresentable() {
    runtime().block_on(async{
    let fx=Fx::new();let(worker,dispatcher)=fx.dispatcher(Source::new(vec![]));fx.refresh(&["openai"]);deliver(&dispatcher).await;fx.refresh(&["openai"]);deliver(&dispatcher).await;
    let legacy=fx.query_schema(json!({"modelIds":["priced-model"],"page":{"limit":50}}),1);assert_eq!(legacy["kind"],"Result","{legacy}");assert_eq!(legacy["result"]["value"]["items"][0]["price"]["inputUsdPerMillion"],"1.25");assert_eq!(legacy["result"]["value"]["items"][0]["price"]["revision"],"1","publication revisions never become override CAS tokens");
    let filtered=fx.query(json!({"modelIds":["priced-model","unpriced-model"],"providerIds":["anthropic"],"includeHistory":true,"page":{"limit":50}}));let view=&filtered["result"]["value"];assert_eq!(view["models"].as_array().unwrap().len(),1);assert_eq!(view["models"][0]["modelId"],"unpriced-model");assert!(view["items"].as_array().unwrap().is_empty());assert!(view["providers"].as_array().unwrap().iter().all(|p|p["providerId"]=="anthropic"));
    let raw=fx.frame("router.price_override",3,"Command",json!({"modelId":"priced-model","expectedRevision":"1","change":{"kind":"Set","tariffs":[{"upstreamModelId":"priced-model","serviceTier":"Priority","modality":"Audio","chargeKind":"Input","unit":"Second","units":"1","amountUsd":"0.0125"}]}}));applied(fx.command(&fx.user,"router.price_override",&raw,WriteOptions::default()));let legacy=fx.query_schema(json!({"modelIds":["priced-model"],"page":{"limit":50}}),1);assert_ne!(legacy["kind"],"Result","exact dimensions cannot become legacy zero shadows: {legacy}");assert!(legacy.to_string().contains("UnsupportedCapability"),"{legacy}");worker.drain().await;dispatcher.stop();
});
}

#[test]
fn accepted_refresh_cancelled_before_dispatch_does_not_leave_a_permanent_pending_provider() {
    runtime().block_on(async {
        let fx = Fx::new();
        let (_, op) = fx.refresh(&["openai"]);
        let cancelled_op = op.clone();
        write(&*fx.store, move |tx| {
            let page = tx.queue().deliveries(
                &DeliveryFilter {
                    consumer: Some(refresh::CONSUMER.into()),
                    operation_id: Some(cancelled_op.clone()),
                },
                None,
                50,
            )?;
            assert_eq!(page.entries.len(), 1);
            tx.queue().cancel(&page.entries[0].id)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            read(&*fx.store, |s| s.operations().operation(&op))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::CancelledBeforeDispatch
        );
        let source = Source::new(vec![]);
        let (worker, dispatcher) = fx.dispatcher(source.clone());
        worker.tick().await.unwrap();
        let status = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
        assert!(!status.refresh_pending);
        assert!(
            status
                .last_error
                .unwrap()
                .contains("CancelledBeforeDispatch")
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 0);
        assert!(read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).is_none());
        worker.drain().await;
        dispatcher.stop();
    });
}
struct MalformedSource(usize);
impl PriceSource for MalformedSource {
    fn fetch(
        &self,
        p: OfficialPriceProvider,
        now: i64,
    ) -> Result<FetchedPriceCatalog, PriceFetchError> {
        let mut c = catalog(p, now, "1.25");
        match self.0 {
            0 => c.source_url = "https://untrusted.example/prices".into(),
            1 => c.source_digest = "invalid".into(),
            2 => c.tariffs.push(c.tariffs[0].clone()),
            3 => c.verified_at_ms = now + 1,
            4 => c.tariffs.clear(),
            _ => c.tariffs[0].amount_usd = "NaN".into(),
        }
        Ok(c)
    }
}
#[test]
fn invalid_or_ambiguous_public_sources_are_visible_failures_and_never_partial_snapshots() {
    runtime().block_on(async {
        for case in 0..6 {
            let fx = Fx::new();
            let (worker, dispatcher) = fx.dispatcher(Arc::new(MalformedSource(case)));
            let (_, op) = fx.refresh(&["openai"]);
            deliver(&dispatcher).await;
            let status = read(&*fx.store, |s| s.usage().price_status("openai")).unwrap();
            assert!(!status.refresh_pending, "case{case}");
            assert!(status.last_error.is_some(), "case{case}");
            assert!(read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).is_none());
            assert_eq!(
                read(&*fx.store, |s| s.operations().operation(&op))
                    .unwrap()
                    .state,
                devworks_core::protocol::OperationState::Failed
            );
            worker.drain().await;
            dispatcher.stop();
        }
    });
}

struct NativeAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn devworks_core::ports::Clock>,
}
impl devworks_core::ports::AuthorityPort for NativeAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    source: Arc<dyn PriceSource>,
) -> devworks_engine::Composition {
    use devworks_adapters::{
        fs::{NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources},
        power::NativePower,
        process::NativeProcessHost,
    };
    use devworks_core::services::{
        artifacts::ArtifactService, diagnostics::DiagnosticService, resources::ResourceService,
        runs::ControlService,
    };
    let mut config = devworks_engine::RuntimeConfig::new(id("native-price-runtime"));
    let mut prices = devworks_engine::prices::PriceComposition::new(source);
    prices.interval = std::time::Duration::from_secs(3600);
    config.price_refresh = Some(prices);
    devworks_engine::Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(NativeAuthority {
            store: store.clone(),
            clock: h.clock(),
        }),
        artifacts: Arc::new(ArtifactService::new(
            Arc::new(NativeArtifactFiles::new(
                h.root.clone(),
                h.root.with_extension("cache"),
            )),
            Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock(),
            h.ids.clone(),
        )),
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        runs: devworks_engine::RunComposition::new(Arc::new(ControlService::new(h.ids.clone()))),
        power: Arc::new(NativePower),
        diagnostics: Arc::new(DiagnosticService::new(
            store,
            h.ids.clone(),
            h.clock(),
            None,
        )),
        browser: None,
        git: None,
        packages: None,
        use_cases: UseCases::new(),
        consumers: vec![],
        indexing: None,
        config,
    }
}
#[test]
fn runtime_boot_and_lost_wake_catch_up_once_then_shutdown_fences_further_maintenance() {
    runtime().block_on(async {
        let h = Harness::new();
        let store = h.open();
        principal(&*store, "user", PrincipalKind::Human);
        seed(&*store, &h);
        let source = Source::new(vec![]);
        let app = devworks_engine::boot(composition(&h, store.clone(), source.clone()))
            .await
            .unwrap();
        let mut changes = app.dispatcher().subscribe();
        within("runtime official price catchup", async {
            while read(&*store, |s| s.usage().current_price_snapshot("openai")).is_none()
                || read(&*store, |s| s.usage().current_price_snapshot("anthropic")).is_none()
            {
                changes.changed().await.unwrap();
            }
        })
        .await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        let old = read(&*store, |s| s.usage().current_price_snapshot("openai")).unwrap();
        app.shutdown().await;
        assert_eq!(app.price_refreshes().unwrap().tick().await.unwrap(), 0);
        drop(app);
        drop(store);
        h.advance(refresh::WEEK_MS * 5);
        let store = h.open();
        let app = devworks_engine::boot(composition(&h, store.clone(), source.clone()))
            .await
            .unwrap();
        let mut changes = app.dispatcher().subscribe();
        within("reopen repairs overdue refresh once", async {
            while read(&*store, |s| s.usage().current_price_snapshot("openai"))
                .unwrap()
                .revision
                < 2
                || read(&*store, |s| s.usage().current_price_snapshot("anthropic"))
                    .unwrap()
                    .revision
                    < 2
            {
                changes.changed().await.unwrap();
            }
        })
        .await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            read(&*store, |s| s.usage().price_snapshot(&old.id)).unwrap(),
            old
        );
        app.price_refreshes().unwrap().notify();
        app.price_refreshes().unwrap().tick().await.unwrap();
        assert_eq!(source.calls.load(Ordering::SeqCst), 4);
        app.shutdown().await;
        assert_eq!(app.price_refreshes().unwrap().tick().await.unwrap(), 0);
    });
}

#[test]
fn periodic_runtime_pass_repairs_a_lost_wakeup_without_replaying_missed_weeks() {
    runtime().block_on(async {
        let h = Harness::new();
        let store = h.open();
        principal(&*store, "user", PrincipalKind::Human);
        seed(&*store, &h);
        let source = Source::new(vec![]);
        let mut config = composition(&h, store.clone(), source.clone());
        config.config.price_refresh.as_mut().unwrap().interval =
            std::time::Duration::from_millis(5);
        let app = devworks_engine::boot(config).await.unwrap();
        let mut changed = app.dispatcher().subscribe();
        within("initial price snapshots", async {
            while read(&*store, |s| s.usage().current_price_snapshot("openai")).is_none()
                || read(&*store, |s| s.usage().current_price_snapshot("anthropic")).is_none()
            {
                changed.changed().await.unwrap();
            }
        })
        .await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        h.advance(refresh::WEEK_MS * 3);
        within("timer repairs missing maintenance hint", async {
            while read(&*store, |s| s.usage().current_price_snapshot("openai"))
                .unwrap()
                .revision
                < 2
                || read(&*store, |s| s.usage().current_price_snapshot("anthropic"))
                    .unwrap()
                    .revision
                    < 2
            {
                changed.changed().await.unwrap();
            }
        })
        .await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 4);
        app.shutdown().await;
    });
}

struct GateOpenAi {
    gate: Arc<Gate>,
}
impl PriceSource for GateOpenAi {
    fn fetch(
        &self,
        p: OfficialPriceProvider,
        now: i64,
    ) -> Result<FetchedPriceCatalog, PriceFetchError> {
        if p == OfficialPriceProvider::OpenAi {
            self.gate.started.notify_one();
            let mut open = self.gate.open.lock().unwrap();
            while !*open {
                open = self.gate.release.wait(open).unwrap();
            }
        }
        Ok(catalog(p, now, "1.25"))
    }
}
#[test]
fn newer_refresh_of_one_published_provider_does_not_erase_an_interrupted_multi_provider_operation()
{
    runtime().block_on(async {
        let fx = Fx::new();
        let gate = Gate::new();
        let (worker, dispatcher) = fx.dispatcher(Arc::new(GateOpenAi { gate: gate.clone() }));
        let (_, old) = fx.refresh(&["anthropic", "openai"]);
        dispatcher.recover().await.unwrap();
        dispatcher.tick().await.unwrap();
        within(
            "second provider waits after first publication",
            gate.started.notified(),
        )
        .await;
        let first = read(&*fx.store, |s| {
            s.usage().current_price_snapshot("anthropic")
        })
        .unwrap();
        let (_, newer) = fx.refresh(&["anthropic"]);
        assert_ne!(old, newer);
        worker.stop();
        dispatcher.stop();
        let draining = tokio::spawn({
            let worker = worker.clone();
            async move { worker.drain().await }
        });
        gate.release();
        draining.await.unwrap();
        within(
            "old multi-provider operation settles unknown",
            dispatcher.idle(),
        )
        .await;
        assert_eq!(
            read(&*fx.store, |s| s.operations().operation(&old))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::OutcomeUnknown
        );
        drop(dispatcher);
        drop(worker);
        let Fx {
            h,
            store,
            service,
            cases,
            user: _,
            agent: _,
        } = fx;
        drop(service);
        drop(cases);
        drop(store);
        let store = h.open();
        let service = Arc::new(PriceService::new(h.ids.clone(), h.clock(), true));
        let source = Source::new(vec![]);
        let worker = PriceSupervisor::new(store.clone(), service, source.clone());
        let dispatcher = Dispatcher::new(
            store.clone(),
            h.clock(),
            vec![worker.consumer()],
            DispatcherConfig::new(id("multi-price-restart")),
        )
        .unwrap();
        dispatcher.recover().await.unwrap();
        worker.tick().await.unwrap();
        for _ in 0..4 {
            dispatcher.tick().await.unwrap();
            within("multi-provider catchup", dispatcher.idle()).await;
            if read(&*store, |s| s.operations().operation(&newer))
                .is_some_and(|v| is_terminal(v.state))
            {
                break;
            }
        }
        assert_eq!(
            read(&*store, |s| s.operations().operation(&old))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::ObservedComplete
        );
        assert_eq!(
            read(&*store, |s| s.operations().operation(&newer))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::ObservedComplete
        );
        assert_eq!(
            read(&*store, |s| s.usage().current_price_snapshot("anthropic"))
                .unwrap()
                .revision,
            2
        );
        assert_eq!(
            read(&*store, |s| s.usage().current_price_snapshot("openai"))
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            read(&*store, |s| s.usage().price_snapshot(&first.id)).unwrap(),
            first
        );
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            2,
            "recover only unfinished OpenAI plus the actual newer Anthropic refresh"
        );
        worker.drain().await;
        dispatcher.stop();
    });
}

#[test]
fn actual_official_origin_and_explicit_local_policy_qualify_sources_independently_of_protocol_labels()
 {
    runtime().block_on(async{
    let fx=Fx::new();let(worker,dispatcher)=fx.dispatcher(Source::new(vec![]));fx.refresh(&["openai"]);deliver(&dispatcher).await;
    write(&*fx.store,|tx|{let mut c=tx.router().connection(&id("openai-connection"))?.unwrap();let rev=c.revision;c.revision+=1;c.endpoint="https://proxy.custom.example/v1".into();tx.router().put_connection(&c,Some(rev))}).unwrap();let configured=read(&*fx.store,|s|refresh::configured(s.router()));assert!(!configured.contains_key("openai"));let view=fx.query(json!({"modelIds":["priced-model"],"page":{"limit":50}}));assert_eq!(view["result"]["value"]["models"][0]["priceKind"],"Untracked");assert!(view["result"]["value"]["items"].as_array().unwrap().is_empty());
    write(&*fx.store,|tx|{let mut c=tx.router().connection(&id("openai-connection"))?.unwrap();let rev=c.revision;c.revision+=1;c.profile="Embedding".into();c.endpoint="https://api.openai.com/v1".into();tx.router().put_connection(&c,Some(rev))?;let mut m=tx.router().route_model(&id("priced-model"))?.unwrap();let rev=m.revision;m.revision+=1;m.endpoint_profile="Embedding".into();tx.router().put_route_model(&m,Some(rev))}).unwrap();let configured=read(&*fx.store,|s|refresh::configured(s.router()));assert!(configured.contains_key("openai"));let view=fx.query(json!({"modelIds":["priced-model"],"page":{"limit":50}}));assert_eq!(view["result"]["value"]["models"][0]["priceKind"],"Estimated");assert_eq!(view["result"]["value"]["items"].as_array().unwrap().len(),2);
    write(&*fx.store,|tx|{let mut c=tx.router().connection(&id("openai-connection"))?.unwrap();let rev=c.revision;c.revision+=1;c.local_free=true;c.endpoint="https://custom.example".into();tx.router().put_connection(&c,Some(rev))}).unwrap();let view=fx.query_schema(json!({"modelIds":["priced-model"],"page":{"limit":50}}),1);assert_eq!(view["result"]["value"]["items"][0]["priceKind"],"Free");assert!(view["result"]["value"]["items"][0].get("price").is_none());assert_eq!(view["result"]["value"]["items"][0]["stale"],false);let history=fx.query(json!({"modelIds":[],"providerIds":["openai"],"includeHistory":true,"page":{"limit":50}}));assert_eq!(history["result"]["value"]["snapshots"].as_array().unwrap().len(),1,"retired official source history remains inspectable");worker.drain().await;dispatcher.stop();
});
}

#[test]
fn unconfigured_official_publisher_can_be_refreshed_manually_without_enabling_automatic_work_or_credentials()
 {
    runtime().block_on(async {
        let fx = Fx::new();
        write(&*fx.store, |tx| {
            for name in ["openai-connection", "anthropic-connection"] {
                let mut c = tx.router().connection(&id(name))?.unwrap();
                let rev = c.revision;
                c.revision += 1;
                c.enabled = false;
                tx.router().put_connection(&c, Some(rev))?;
            }
            Ok(())
        })
        .unwrap();
        let source = Source::new(vec![]);
        let (worker, dispatcher) = fx.dispatcher(source.clone());
        assert_eq!(worker.tick().await.unwrap(), 0);
        assert!(read(&*fx.store, |s| s.usage().price_status("openai")).is_none());
        let (_, op) = fx.refresh(&["openai"]);
        let pending = fx.query(json!({"modelIds":[],"providerIds":["openai"],"page":{"limit":50}}));
        assert!(
            pending["result"]["value"]["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["providerId"] == "openai" && p["refreshOperationId"] == op.0.as_str())
        );
        deliver(&dispatcher).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert!(read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).is_some());
        assert_eq!(worker.tick().await.unwrap(), 0);
        fx.h.advance(refresh::WEEK_MS * 3);
        assert_eq!(
            worker.tick().await.unwrap(),
            0,
            "only currently configured providers join automatic cadence"
        );
        for name in ["openai-connection", "anthropic-connection"] {
            let c = read(&*fx.store, |s| s.router().connection(&id(name))).unwrap();
            assert!(!c.enabled);
            assert!(c.credential_ref.is_none());
        }
        let view = fx.query(json!({"modelIds":[],"page":{"limit":50}}));
        assert!(
            view["result"]["value"]["providers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["providerId"] == "openai")
        );
        worker.drain().await;
        dispatcher.stop();
    });
}

fn configured_history(fx: &Fx) -> (Vec<Value>, Vec<Value>) {
    let mut cursor = None;
    let mut entries = Vec::new();
    let mut states = Vec::new();
    for _ in 0..30 {
        let mut body = json!({"modelIds":["priced-model"],"providerIds":["user-configured"],"includeHistory":true,"page":{"limit":2}});
        if let Some(saved) = cursor.take() {
            body["page"]["cursor"] = saved;
        }
        let reply = fx.query(body);
        assert_eq!(reply["kind"], "Result", "{reply}");
        let view = &reply["result"]["value"];
        let count = [
            "models",
            "providers",
            "snapshots",
            "items",
            "overrideHistory",
        ]
        .into_iter()
        .map(|key| view[key].as_array().map_or(0, Vec::len))
        .sum::<usize>();
        assert!(count <= 2, "history shares the whole query page bound");
        entries.extend(view["items"].as_array().unwrap().iter().cloned());
        states.extend(view["overrideHistory"].as_array().unwrap().iter().cloned());
        if !view["hasMore"].as_bool().unwrap() {
            return (entries, states);
        }
        cursor = Some(view["nextCursor"].clone());
    }
    panic!("configured history pagination did not finish");
}
#[test]
fn configured_history_preserves_set_a_set_b_clear_authorship_and_original_quotes_without_duplicate_current_rates()
 {
    runtime().block_on(async {
    let fx = Fx::new();
    let (worker,dispatcher)=fx.dispatcher(Source::new(vec![]));fx.refresh(&["openai"]);deliver(&dispatcher).await;
    let published=read(&*fx.store,|s|s.usage().current_price_snapshot("openai")).unwrap();
    let set = |fx: &Fx, expected: &str, amount: &str| {
        let raw=fx.frame("router.price_override",3,"Command",json!({"modelId":"priced-model","expectedRevision":expected,"price":{"inputUsdPerMillion":amount,"outputUsdPerMillion":amount}}));
        applied(fx.command(
            &fx.user,
            "router.price_override",
            &raw,
            WriteOptions::default(),
        ))
    };
    let query = PriceQuery {
        catalog_model_id: id("priced-model"),
        provider_model_id: Some("priced-model".into()),
        connection_id: id("openai-connection"),
        endpoint_profile: "OpenAiResponses".into(),
        access_mode: "Api".into(),
        service_tier: None,
        input_tokens: Some(100),
        cache_write_ttl_ms: None,
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let source_quote=read(&*fx.store,|s|devworks_core::services::prices::CatalogPriceResolver.quote(s,&query));
    assert_eq!(source_quote.classification,CostClassification::OfficialEstimate);
    assert_eq!(source_quote.connection_revision,Some(1));assert_eq!(source_quote.snapshot_id,Some(published.id.clone()));assert_eq!(source_quote.price_revision,Some(published.revision));assert_eq!(source_quote.override_revision,Some(1));assert_eq!(source_quote.tariffs,published.tariffs);
    let source_json=serde_json::to_value(&source_quote).unwrap();
    set(&fx,"1","1.35");
    let original = read(&*fx.store, |s| {
        devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)
    });
    assert_eq!(original.override_revision, Some(2));
    assert_eq!(original.classification,CostClassification::ConfiguredEstimate);assert_eq!(original.connection_revision,Some(1));assert!(original.snapshot_id.is_none());assert!(original.price_revision.is_none());assert_eq!(original.tariffs,read(&*fx.store,|s|s.usage().price_override(&id("priced-model"))).unwrap().tariffs);
    assert_eq!(
        original
            .rates
            .as_ref()
            .unwrap()
            .input_usd_per_million
            .as_deref(),
        Some("1.35")
    );
    let original_json = serde_json::to_value(&original).unwrap();
    let retained = original_json.clone();
    let retained_source=source_json.clone();
    let actor = fx.user.principal().clone();
    let quote_event = write(&*fx.store, move |tx| {
        let event = devworks_core::protocol::EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor,
            project_id: None,
            root_id: None,
            operation_id: None,
            aggregate_kind: "price_quote".into(),
            aggregate_id: "original-quote".into(),
            aggregate_revision: 1,
            kind: "PriceQuoteCaptured".into(),
            payload: json!({"quote":retained,"sourceQuote":retained_source}),
        })?;
        Ok(event)
    })
    .unwrap();
    fx.h.advance(1_000);
    set(&fx, "2", "2.75");
    let first=fx.query(json!({"modelIds":["priced-model"],"providerIds":["user-configured"],"includeHistory":true,"page":{"limit":2}}));
    let old_cursor = first["result"]["value"]["nextCursor"].clone();
    let (entries, states) = configured_history(&fx);
    let combined=fx.query(json!({"modelIds":["priced-model"],"includeHistory":true,"page":{"limit":50}}));
    let combined=&combined["result"]["value"];
    assert_eq!(combined["snapshots"].as_array().unwrap().len(),1);
    assert_eq!(combined["overrideHistory"].as_array().unwrap().len(),2);
    assert_eq!(combined["items"].as_array().unwrap().iter().filter(|entry|entry["origin"]=="UserConfigured").count(),4,"current B is emitted only through immutable configured history");
    assert_eq!(combined["items"].as_array().unwrap().iter().filter(|entry|entry["origin"]=="OfficialSnapshot").count(),2,"official and configured histories are both present");
    assert_eq!(states.len(), 2);
    assert_eq!(
        entries.len(),
        4,
        "two charge dimensions for each of two revisions; current B must not be repeated"
    );
    let unique: std::collections::BTreeSet<_> = entries
        .iter()
        .map(|v| {
            (
                v["overrideRevision"].as_str().unwrap().to_owned(),
                v["tariff"]["chargeKind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(unique.len(), 4);
    assert!(
        entries
            .iter()
            .all(|v| v["providerId"] == "user-configured" && v["modelId"] == "priced-model")
    );
    assert_eq!(states[0]["updatedBy"], "user");
    assert_eq!(states[0]["updatedAtMs"], T0.to_string());
    assert_eq!(states[1]["updatedAtMs"], (T0 + 1_000).to_string());
    let current = fx.query(
        json!({"modelIds":["priced-model"],"providerIds":["user-configured"],"page":{"limit":50}}),
    );
    let view = &current["result"]["value"];
    assert!(view.get("overrideHistory").is_none());
    assert_eq!(view["items"].as_array().unwrap().len(), 2);
    assert!(
        view["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["overrideRevision"] == "3")
    );
    fx.h.advance(1_000);
    let clear = fx.frame(
        "router.price_override",
        3,
        "Command",
        json!({"modelId":"priced-model","expectedRevision":"3","change":{"kind":"Clear"}}),
    );
    applied(fx.command(
        &fx.user,
        "router.price_override",
        &clear,
        WriteOptions::default(),
    ));
    let expired=fx.query(json!({"modelIds":["priced-model"],"providerIds":["user-configured"],"includeHistory":true,"page":{"limit":2,"cursor":old_cursor}}));
    assert_ne!(expired["kind"], "Result");
    let (entries, states) = configured_history(&fx);
    assert_eq!(entries.len(), 4);
    assert_eq!(states.len(), 3);
    assert_eq!(states[2]["kind"], "Cleared");
    assert_eq!(states[2]["revision"], "4");
    assert_eq!(states[2]["updatedBy"], "user");
    assert_eq!(states[2]["updatedAtMs"], (T0 + 2_000).to_string());
    assert!(
        entries.iter().all(|v| v["overrideRevision"] != "4"),
        "Clear is metadata, never a fabricated zero tariff"
    );
    let current = fx.query(json!({"modelIds":["priced-model"],"page":{"limit":50}}));
    assert!(
        current["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .iter().all(|entry|entry["origin"]!="UserConfigured")
    );
    assert_eq!(current["result"]["value"]["models"][0]["revision"], "4");
    let now = read(&*fx.store, |s| {
        devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)
    });
    assert_eq!(now.classification,CostClassification::OfficialEstimate);assert_eq!(now.snapshot_id,source_quote.snapshot_id);assert_eq!(now.price_revision,source_quote.price_revision);assert_eq!(now.override_revision,Some(4));assert_eq!(now.rates,source_quote.rates);assert_eq!(now.tariffs,source_quote.tariffs);
    assert_eq!(
        read(&*fx.store, |s| s.history().event(&quote_event))
            .unwrap()
            .payload["quote"],
        original_json
    );
    let official_only=fx.query(json!({"modelIds":["priced-model"],"providerIds":["openai"],"includeHistory":true,"page":{"limit":50}}));
    assert!(
        official_only["result"]["value"]["overrideHistory"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        official_only["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["origin"] != "UserConfigured")
    );
    let wrong_model=fx.query(json!({"modelIds":["unpriced-model"],"providerIds":["user-configured"],"includeHistory":true,"page":{"limit":50}}));
    assert!(
        wrong_model["result"]["value"]["overrideHistory"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    worker.drain().await;dispatcher.stop();drop(dispatcher);drop(worker);
    let Fx {
        h,
        store,
        service,
        cases,
        user,
        agent,
    } = fx;
    drop(store);
    let store = h.open();
    let fx = Fx {
        h,
        store,
        service,
        cases,
        user,
        agent,
    };
    let (entries, states) = configured_history(&fx);
    assert_eq!((entries.len(), states.len()), (4, 3));
    assert_eq!(fx.h.now(), T0 + 2_000);
    assert_eq!(
        read(&*fx.store, |s| s.history().event(&quote_event))
            .unwrap()
            .payload["quote"],
        original_json
    );
    assert_eq!(read(&*fx.store,|s|s.history().event(&quote_event)).unwrap().payload["sourceQuote"],source_json);
    });
}

#[test]
fn runtime_shutdown_bounds_stalled_price_fetches_and_fences_every_late_write() {
    runtime().block_on(within("bounded price-source shutdown", async {
        struct Release(Arc<Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.release();
            }
        }
        for recovering in [false, true] {
            let fx = Fx::new();
            let (first_worker, first_dispatcher) = fx.dispatcher(Source::new(vec![]));
            fx.refresh(&["openai"]);
            deliver(&first_dispatcher).await;
            let last_good =
                read(&*fx.store, |s| s.usage().current_price_snapshot("openai")).unwrap();
            first_worker.drain().await;
            first_dispatcher.stop();
            drop(first_dispatcher);
            drop(first_worker);

            let (_, operation) = fx.refresh(&["openai"]);
            if recovering {
                let gate = Gate::new();
                let _release = Release(gate.clone());
                let source = Arc::new(Source {
                    outcomes: Mutex::new(VecDeque::new()),
                    calls: AtomicUsize::new(0),
                    gate: Some(gate.clone()),
                });
                let (worker, dispatcher) = fx.dispatcher(source);
                dispatcher.recover().await.unwrap();
                dispatcher.tick().await.unwrap();
                gate.started.notified().await;
                worker.stop();
                gate.release();
                worker.drain().await;
                dispatcher.idle().await;
                assert_eq!(
                    read(&*fx.store, |s| s.operations().operation(&operation))
                        .unwrap()
                        .state,
                    devworks_core::protocol::OperationState::OutcomeUnknown
                );
                dispatcher.stop();
            }
            let gate = Gate::new();
            let source = Arc::new(Source {
                outcomes: Mutex::new(VecDeque::new()),
                calls: AtomicUsize::new(0),
                gate: Some(gate.clone()),
            });
            let mut composed = composition(&fx.h, fx.store.clone(), source.clone());
            composed.config.shutdown_bound = std::time::Duration::from_millis(100);
            let app = devworks_engine::boot(composed).await.unwrap();
            let _release = Release(gate.clone());
            gate.started.notified().await;
            let began = std::time::Instant::now();
            tokio::time::timeout(std::time::Duration::from_secs(2), app.shutdown())
                .await
                .expect("price fetch exceeded the shutdown bound");
            assert!(
                began.elapsed() < std::time::Duration::from_secs(2),
                "shutdown waited for the price source"
            );
            let snapshot = || {
                read(&*fx.store, |s| {
                    Ok((
                        s.stamp().clone(),
                        s.usage().price_revision()?,
                        s.usage().current_price_snapshot("openai")?,
                        s.usage().current_price_snapshot("anthropic")?,
                        s.usage().price_status("openai")?,
                        s.usage().price_status("anthropic")?,
                        s.operations().operation(&operation)?,
                        s.queue()
                            .deliveries(
                                &DeliveryFilter {
                                    consumer: Some(refresh::CONSUMER.into()),
                                    operation_id: None,
                                },
                                None,
                                64,
                            )?
                            .entries,
                    ))
                })
            };
            let before = snapshot();
            assert_eq!(before.2, Some(last_good));
            assert_eq!(
                before.6.as_ref().unwrap().state,
                devworks_core::protocol::OperationState::OutcomeUnknown
            );
            assert!(before.4.as_ref().unwrap().refresh_pending);
            gate.release();
            // This graceful wait acknowledges the actual detached callback returning; it is
            // deliberately outside Runtime.shutdown's shared deadline.
            app.price_refreshes().unwrap().drain().await;
            app.dispatcher().idle().await;
            assert_eq!(
                snapshot(),
                before,
                "late source changed catalog, operation or delivery after shutdown"
            );
            assert_eq!(app.price_refreshes().unwrap().tick().await.unwrap(), 0);
            assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        }
    }));
}

#[test]
fn a_stalled_price_source_does_not_keep_the_tokio_runtime_alive_after_app_shutdown() {
    let h = Harness::new();
    let store = h.open();
    principal(&*store, "user", PrincipalKind::Human);
    seed(&*store, &h);
    let gate = Gate::new();
    let source = Arc::new(Source {
        outcomes: Mutex::new(VecDeque::new()),
        calls: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let mut composed = composition(&h, store.clone(), source);
    composed.config.shutdown_bound = std::time::Duration::from_millis(100);
    let (dropped, after_drop) = std::sync::mpsc::channel();
    let source_gate = gate.clone();
    let host = std::thread::spawn(move || {
        let rt = runtime();
        let app = rt.block_on(devworks_engine::boot(composed)).unwrap();
        rt.block_on(source_gate.started.notified());
        rt.block_on(app.shutdown());
        drop(rt);
        dropped.send(app).unwrap();
    });
    let app = match after_drop.recv_timeout(std::time::Duration::from_secs(3)) {
        Ok(app) => app,
        Err(error) => {
            gate.release();
            host.join().unwrap();
            panic!("stalled source held Tokio runtime destruction: {error}");
        }
    };
    host.join().unwrap();
    let before = read(&*store, |s| {
        Ok((
            s.stamp().clone(),
            s.usage().price_status("openai")?,
            s.usage().price_status("anthropic")?,
        ))
    });
    gate.release();
    runtime().block_on(app.price_refreshes().unwrap().drain());
    assert_eq!(
        read(&*store, |s| Ok((
            s.stamp().clone(),
            s.usage().price_status("openai")?,
            s.usage().price_status("anthropic")?
        ))),
        before
    );
}
