//! Actual native provider → volatile preview → canonical transcript; controlled HTTP bytes,
//! no model execution and no successful fake backend.
mod common;
use common::*;
use devworks_core::{
    ports::{
        IdKind,
        http::HttpRequest,
        provider_http::*,
        storage::*,
        usage::{PriceQuery, PriceResolver},
    },
    protocol::{Counter, Id, Protocol, Revision, provider::*},
    providers::context::*,
    services::{
        router::{
            Purpose, RequestAdmission,
            accounting::{AttemptSelection, ReportCategory},
        },
        usage::{accounting::AttemptRequest, meter::InputSemantics, pricing::PricingContext},
    },
};
use devworks_desktop::{host::Engine, ops, shell::Shell};
use devworks_engine::providers::driver::ProviderComposition;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
fn id(s: &str) -> Id {
    s.to_owned().try_into().unwrap()
}
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    wake: Condvar,
}
impl Gate {
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.wake.notify_all();
    }
    fn wait_close(&self) {
        let until = Instant::now() + Duration::from_secs(10);
        let mut state = self.state.lock().unwrap();
        while !state.0 {
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "producer reaches physical close");
            state = self.wake.wait_timeout(state, remaining).unwrap().0;
        }
    }
}
struct Release(Arc<Gate>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct Http(Arc<Gate>);
struct Stream {
    gate: Arc<Gate>,
    head: ResponseHead,
    bytes: std::io::Cursor<Vec<u8>>,
}
impl StreamCloser for Gate {
    fn close(&self) -> Result<(), TransportError> {
        self.release();
        Ok(())
    }
}
impl StreamingHttp for Http {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        assert_eq!(request.url, "http://localhost:11434/v1/chat/completions");
        Ok(Box::new(Stream { gate: self.0.clone(), head: ResponseHead { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())] },
            bytes: std::io::Cursor::new(concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Visible before physical close\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4}}\n\n",
                "data: [DONE]\n\n").as_bytes().to_vec()) }))
    }
}
impl HttpStream for Stream {
    fn closer(&self) -> Arc<dyn StreamCloser> {
        self.gate.clone()
    }
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn read(&mut self, out: &mut [u8], _: &Cancellation) -> Result<usize, TransportError> {
        Ok(std::io::Read::read(&mut self.bytes, out).unwrap())
    }
    fn close(&mut self) -> Result<(), TransportError> {
        let mut state = self.gate.state.lock().unwrap();
        state.0 = true;
        self.gate.wake.notify_all();
        while !state.1 {
            state = self.gate.wake.wait(state).unwrap();
        }
        Ok(())
    }
}
fn write<T: Send + 'static>(
    s: &dyn ApplicationStore,
    f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
) -> T {
    run_write(s, WriteOptions::default(), f).unwrap()
}
fn start_producer(engine: &Engine, session: Id) -> std::thread::JoinHandle<()> {
    let runtime = engine.host().runtime().clone();
    let store = runtime.store().clone();
    // The preview producer acts as the recorded session owner; the Human viewer is its creator.
    let agent = read_snapshot(store.as_ref(), &ReadOptions::default(), |read| {
        read.sessions()
            .session(&session)?
            .map(|row| row.agent_id)
            .ok_or(StorageError::ScopeDenied)
    })
    .unwrap();
    let info = store.info();
    let ctx = devworks_core::context::HostIngress::new(store.as_ref())
        .context(
            agent,
            devworks_core::context::PrincipalKind::Agent,
            id("provider-invocation"),
            id("provider-output"),
            Counter::new(info.restore_epoch).unwrap(),
            engine.host().user().policy_revision().clone(),
        )
        .with_session(session.clone());
    write(store.as_ref(), move |tx| {
        let now = tx.now_ms();
        tx.router().put_connection(
            &StoredConnection {
                id: id("connection"),
                name: "Controlled local provider".into(),
                profile: "CompatibleChat".into(),
                endpoint: "http://localhost:11434/v1".into(),
                enabled: true,
                local_free: true,
                open_router: None,
                credential_ref: None,
                credential_revision: 0,
                account_revision: 1,
                refresh_operation_id: None,
                credential_epoch: None,
                lifecycle: "Connected".into(),
                state_reason: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: Some(vec![DiscoveredModel {
                    id: "fixture-model".into(),
                    context_tokens: Some(10000),
                }]),
                discovered_at_ms: Some(now),
                healthy: Some(true),
                checked_at_ms: Some(now),
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
            },
            None,
        )?;
        tx.router().put_route_model(
            &StoredRouteModel {
                id: id("model"),
                display_name: "Controlled model".into(),
                endpoint_profile: "CompatibleChat".into(),
                upstream_model_id: "fixture-model".into(),
                enabled: true,
                efforts: vec![],
                image_input: false,
                tools: false,
                context_tokens: Some(10000),
                connection_id: Some(id("connection")),
                position: 1,
                revision: 1,
                updated_at_ms: now,
            },
            None,
        )
    });
    let context = ProviderContext {
        version: 1,
        instructions: vec![],
        instruction_stamps: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "fixture".into(),
                cache: None,
            }],
        }],
        parameters: GenerationParameters::default(),
    };
    let artifacts = runtime.artifacts().clone();
    let staged = artifacts
        .stage_host_bytes(
            store.as_ref(),
            &serde_json::to_vec(&context).unwrap(),
            json!({"fixture":"input"}),
        )
        .unwrap()
        .unwrap();
    let f = staged.clone();
    let a = artifacts.clone();
    let version = write(store.as_ref(), move |tx| {
        a.commit_host_bytes(
            tx,
            &f,
            "Controlled input",
            "application/json",
            json!({"fixture":true}),
        )
    });
    artifacts.settle_host_bytes(store.as_ref(), &staged, true);
    let request = InferenceRequest {
        protocol: Protocol::Draft1,
        request_id: id("request"),
        purpose: InferencePurpose::Conversation,
        attribution: Attribution::Workspace {
            session_id: session.clone(),
        },
        authority_revision: Revision::new(1).unwrap(),
        control_generation: Revision::new(1).unwrap(),
        instruction_revision: Revision::new(1).unwrap(),
        cancellation_revision: Revision::new(1).unwrap(),
        options: InferenceOptions {
            selection: ModelSelection::Model {
                model_id: id("model"),
            },
            effort: None,
            output_schema: None,
            deadline_ms: 15000,
        },
        input: InferenceInput::Generation {
            session_id: session.clone(),
            context: devworks_core::services::artifacts::version_ref(&version),
            context_epoch: Counter::new(0).unwrap(),
            tools: vec![],
            response_headroom_tokens: Revision::new(10).unwrap(),
            requires_images: false,
            requires_parallel_tools: false,
        },
    };
    let query = PriceQuery {
        catalog_model_id: id("model"),
        provider_model_id: Some("fixture-model".into()),
        connection_id: id("connection"),
        endpoint_profile: "CompatibleChat".into(),
        access_mode: "Api".into(),
        service_tier: None,
        input_tokens: Some(10),
        cache_write_ttl_ms: None,
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let (quote, generation) = read_snapshot(store.as_ref(), &ReadOptions::default(), |s| {
        Ok((
            devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)?,
            s.usage().price_revision()?,
        ))
    })
    .unwrap();
    let attempt = AttemptRequest {
        admission_key: id("admission"),
        request_id: id("request"),
        actor_id: ctx.principal().clone(),
        work_id: None,
        job_run_id: None,
        role: "WorkspaceCoordinator".into(),
        query,
        quote,
        price_generation: generation,
        override_revision: 1,
        pricing: PricingContext {
            input_semantics: InputSemantics::Unknown,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        },
        selection: AttemptSelection::Conversation {
            intent: devworks_core::protocol::records::RouteIntent::Model {
                model_id: id("model"),
            },
            effort: devworks_core::protocol::records::EffortName::Default,
            required_capabilities: vec![],
            pin: None,
        },
        input_tokens: 10,
        output_headroom: 10,
        unit_headroom: vec![],
        expected_context_epoch: Some(0),
        dispatch: None,
        cache_diagnostics: vec![],
    };
    let logical = RequestAdmission {
        request_id: id("request"),
        purpose: Purpose::Conversation,
        session_id: Some(session),
        project_id: None,
        root_id: None,
        model_id: id("model"),
        connection_id: id("connection"),
        pin_id: None,
        args_digest: "a".repeat(64),
    };
    let driver = runtime.providers().unwrap().clone();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let invocation = driver
                    .produce(ctx, logical, ReportCategory::Assistant, attempt, request)
                    .await
                    .unwrap();
                assert_eq!(
                    invocation.result.outcome,
                    Some(devworks_core::services::usage::accounting::AttemptEnd::Completed)
                );
            });
    })
}
fn until(
    h: &mut egui_kittest::Harness<'_, Shell>,
    predicate: impl Fn(&egui_kittest::Harness<'_, Shell>) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.step();
        assert!(h.state().wait_idle());
        h.step();
        if predicate(h) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native preview state arrives: selected={:?}, preview={:?}, transcripts={:?}, labels={:?}",
            h.state().chat.selected,
            h.state().chat.text_preview,
            h.state()
                .chat
                .transcripts
                .iter()
                .map(|(k, t)| (k, t.loaded, t.error.as_ref(), t.entries.len()))
                .collect::<Vec<_>>(),
            labels(h, eframe::egui::accesskit::Role::Label)
        );
        std::thread::yield_now();
    }
}
fn agent_session(engine: &Engine) -> Id {
    let store = engine.host().runtime().store().clone();
    let user = engine.host().user().clone();
    let agent = devworks_core::protocol::ActorId(id("preview-agent"));
    write(store.as_ref(), move |tx| {
        tx.actors().register(
            &agent,
            devworks_core::context::PrincipalKind::Agent,
            "Preview agent",
        )?;
        devworks_core::authority::service::bind_principal(
            tx,
            &user,
            devworks_core::ports::storage::NewPrincipalBinding {
                actor: agent.clone(),
                kind: devworks_core::context::PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::WorkspaceCoordinator),
                project_id: None,
                tool_families: None,
                bound_by: user.principal().clone(),
            },
            None,
        )
        .map_err(|error| StorageError::InvalidRecord(format!("{error:?}")))?;
        let session = StoredSession {
            id: tx.new_id(IdKind::Session),
            conversation_id: tx.new_id(IdKind::Conversation),
            agent_id: agent,
            project_id: None,
            root_id: None,
            title: "Streaming chat".into(),
            context_epoch: 0,
            revision: 1,
            created_by: user.principal().clone(),
            created_at_ms: tx.now_ms(),
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        };
        tx.sessions().insert_session(&session)?;
        Ok(session.id)
    })
}
#[test]
fn native_stream_is_visible_before_close_and_replaced_once_by_canonical_history() {
    let root = Root::new("live-preview");
    let gate = Arc::new(Gate::default());
    let mut cfg = config(&root.0);
    cfg.connection_tick = None;
    cfg.providers = Some(ProviderComposition::new(Arc::new(Http(gate.clone()))));
    let engine = Engine::start(cfg).unwrap();
    let _release = Release(gate.clone());
    let b = engine.backend();
    let session = agent_session(&engine);
    let other = ops::started(b.command("sessions.start", &ops::session_start(&*b, "Other chat")))
        .unwrap()
        .id;
    let mut s = shell(&engine, &root, Script::default());
    s.chat.selected = Some(session.as_str().into());
    let mut h = harness(s, 1200.0, 900.0);
    settle(&mut h);
    let producer = start_producer(&engine, session.clone());
    gate.wait_close();
    assert!(
        ops::transcript_page(b.query(
            "sessions.transcript",
            &ops::transcript(&*b, session.as_str(), None)
        ))
        .unwrap()
        .entries
        .is_empty()
    );
    until(&mut h, |h| has_text(h, "Visible before physical close"));
    assert!(has_text(&h, "Streaming response"));
    assert!(
        b.text_preview(&id("missing-session")).is_err(),
        "normal session authorization remains required"
    );
    h.state_mut().chat.selected = Some(other.as_str().into());
    settle(&mut h);
    assert!(!has_text(&h, "Visible before physical close"));
    h.state_mut().chat.selected = Some(session.as_str().into());
    until(&mut h, |h| has_text(h, "Visible before physical close"));
    gate.release();
    producer.join().unwrap();
    until(&mut h, |h| {
        let count = h
            .query_all_by_label("Visible before physical close")
            .count();
        assert!(count <= 1, "handover never duplicates visible text");
        count == 1 && !has_text(h, "Streaming response")
    });
    let page = ops::transcript_page(b.query(
        "sessions.transcript",
        &ops::transcript(&*b, session.as_str(), None),
    ))
    .unwrap();
    assert_eq!(
        page.entries
            .iter()
            .filter(|e| matches!(e.body, TranscriptBody::AssistantAttempt { .. }))
            .count(),
        1
    );
    assert!(h.state().chat.text_preview.is_none());
    let _ = h.get_by_label("Visible before physical close");
}
