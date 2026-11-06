//! Native HTTP -> trusted response head -> source -> owned sink -> router feedback.
//! Build the SQLite fixture before starting the server's bounded request wait: unrelated
//! parallel store initialization is not a provider transport timeout.
use super::*;
use devworks_adapters::provider_stream::NativeStreamingHttp;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};
struct LocalHttp {
    endpoint: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    worker: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}
impl LocalHttp {
    fn new(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
        Self::sequence(vec![response(status, "application/json", headers, body)])
    }
    fn sequence(responses: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("local response not consumed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                captured.lock().unwrap().push(request(&mut stream));
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            endpoint: format!("http://{address}/v1"),
            requests,
            worker: Some(worker),
            stop,
        }
    }
    fn finish(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
impl Drop for LocalHttp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn response(status: u16, content_type: &str, headers: &[(&str, &str)], body: &str) -> String {
    format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{body}",
        body.len(),
        headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>()
    )
}
fn request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 65536);
        assert_eq!(stream.read(&mut byte).unwrap(), 1);
        bytes.push(byte[0]);
    }
    let count = String::from_utf8_lossy(&bytes)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; count];
    stream.read_exact(&mut body).unwrap();
    bytes.extend(body);
    bytes
}
fn native(f: &mut Fx, endpoint: &str) {
    let endpoint = endpoint.to_owned();
    write(f.store.as_ref(), move |tx| {
        let mut connection = tx.router().connection(&id("connection"))?.unwrap();
        let expected = connection.revision;
        connection.endpoint = endpoint;
        if let Some(risk) = connection.risk.as_mut() {
            risk.binding = devworks_core::services::router::connections::profiles::risk_binding(
                &connection.profile,
                &connection.endpoint,
                connection.account_digest.as_deref(),
            );
        }
        tx.router()
            .put_connection(&connection, Some(expected))
            .map(|_| ())
    })
    .unwrap();
    f.driver = NativeProviders::new(
        f.store.clone(),
        f.artifacts.clone(),
        f.connections.clone(),
        f.usage.clone(),
        f.driver.schemas().clone(),
        ProviderComposition::new(Arc::new(NativeStreamingHttp::direct().unwrap())),
    );
}
#[test]
fn real_http_subscription_windows_create_exact_router_cooldown_without_fallback_reset() {
    runtime().block_on(async{
        for (profile,headers,until) in [
            ("CodexSubscriptionResponses",vec![("x-codex-primary-used-percent","100"),("x-codex-primary-reset-after-seconds","37")],T0+37_000),
            ("ClaudeSubscriptionMessages",vec![("anthropic-ratelimit-unified-5h-status","rejected"),("anthropic-ratelimit-unified-5h-reset","1700000041")],T0+41_000),
        ]{
            let mut f=Fx::new(profile,Script::sse(""),false);
            let server=LocalHttp::new(429,&headers,r#"{"error":{"code":"rate_limit_error","message":"subscription window exhausted"}}"#);
            native(&mut f,&server.endpoint);
            let result=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(result.result.outcome,Some(AttemptEnd::Failed));
            assert_eq!(result.result.retry_at_ms,Some(until),"{profile}: only actual HTTP subscription headers can provide this reset");
            let cooldowns=read(f.store.as_ref(),|s|s.router().cooldowns());assert_eq!(cooldowns.len(),1);assert_eq!(cooldowns[0].model_id,f.model);assert_eq!(cooldowns[0].until_ms,until);
            let mut next=f.generation(vec![]);next.request_id=id("next-exhausted-request");
            let blocked=f.produce(next).await.unwrap();assert!(matches!(blocked.accounting,DriveOutcome::Refused(_)),"the router must refuse a fresh allocation while the recorded window is cooling: {:?}",blocked.accounting);
            assert_eq!(server.requests.lock().unwrap().len(),1,"failed inference was never replayed");
            let connection=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();assert_eq!(connection.lifecycle,"Connected");
            assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty(),"no scripted ResponseHead bypassed the real adapter");server.finish();
        }
    });
}

fn other_model(f: &Fx) {
    write(f.store.as_ref(), |tx| {
        let mut row = tx.router().route_model(&id("model"))?.unwrap();
        row.id = id("entitled-model");
        row.upstream_model_id = "entitled-model".into();
        row.display_name = "Other entitled model".into();
        row.position += 1;
        tx.router().put_route_model(&row, None)?;
        let mut connection = tx.router().connection(&id("connection"))?.unwrap();
        let expected = connection.revision;
        connection
            .discovered
            .as_mut()
            .unwrap()
            .push(DiscoveredModel {
                id: "entitled-model".into(),
                context_tokens: Some(10000),
            });
        tx.router()
            .put_connection(&connection, Some(expected))
            .map(|_| ())
    })
    .unwrap();
}
async fn produce_model(
    f: &Fx,
    model: &str,
    upstream: &str,
    request_id: &str,
) -> Result<devworks_engine::providers::driver::ProviderInvocation, StorageError> {
    let mut request = f.generation(vec![]);
    request.request_id = id(request_id);
    request.options.selection = ModelSelection::Model {
        model_id: id(model),
    };
    let query = PriceQuery {
        catalog_model_id: id(model),
        provider_model_id: Some(upstream.into()),
        connection_id: id("connection"),
        endpoint_profile: f.profile.clone(),
        access_mode: "Api".into(),
        service_tier: None,
        input_tokens: Some(10),
        cache_write_ttl_ms: None,
        processing_region: None,
        required_modalities: vec!["Text".into()],
    };
    let quote = read(f.store.as_ref(), |s| {
        devworks_core::services::prices::CatalogPriceResolver.quote(s, &query)
    });
    let logical = RequestAdmission {
        request_id: request.request_id.clone(),
        purpose: Purpose::Conversation,
        session_id: Some(id("session")),
        project_id: None,
        root_id: None,
        model_id: id(model),
        connection_id: id("connection"),
        pin_id: None,
        args_digest: devworks_core::fingerprint::sha256_hex(request_id.as_bytes()),
    };
    let attempt = AttemptRequest {
        admission_key: id(&format!("{request_id}-admission")),
        request_id: request.request_id.clone(),
        actor_id: f.context.principal().clone(),
        work_id: None,
        job_run_id: None,
        role: "Host".into(),
        query,
        quote,
        price_generation: read(f.store.as_ref(), |s| s.usage().price_revision()),
        override_revision: 1,
        pricing: PricingContext {
            input_semantics: InputSemantics::Unknown,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        },
        selection: AttemptSelection::Conversation {
            intent: devworks_core::protocol::records::RouteIntent::Model {
                model_id: id(model),
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
    f.driver
        .produce(
            f.context.clone(),
            logical,
            ReportCategory::Assistant,
            attempt,
            request,
        )
        .await
}
fn success(profile: &str) -> String {
    if profile == "CodexSubscriptionResponses" {
        format!(
            "event: response.completed\ndata: {}\n\n",
            json!({"type":"response.completed","response":{"id":"entitled-response","model":"entitled-model","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}})
        )
    } else {
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"entitled-message\",\"model\":\"entitled-model\",\"usage\":{\"input_tokens\":1}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into()
    }
}
#[test]
fn model_specific_403_preserves_current_credential_and_another_model_really_dispatches() {
    runtime().block_on(async{
        for profile in ["CodexSubscriptionResponses","ClaudeSubscriptionMessages"] {
            for code in ["usage_not_included","unsupported_model"] {
                let headers=if profile=="CodexSubscriptionResponses"{vec![("x-codex-primary-used-percent","100"),("x-codex-primary-reset-after-seconds","37")]}else{vec![("anthropic-ratelimit-unified-5h-status","rejected"),("anthropic-ratelimit-unified-5h-reset","1700000041")]};
                let body=json!({"error":{"code":code,"message":"model entitlement missing despite rate limit wording"}}).to_string();
                let mut f=Fx::new(profile,Script::sse(""),false);
                let server=LocalHttp::sequence(vec![response(403,"application/json",&headers,&body),response(200,"text/event-stream",&[],&success(profile))]);
                native(&mut f,&server.endpoint);other_model(&f);
                let before=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();
                let first=f.produce(f.generation(vec![])).await.unwrap();assert_eq!(first.result.outcome,Some(AttemptEnd::Failed));assert!(first.result.faults.contains(&format!("ModelUnavailable:{code}")),"{:?}",first.result.faults);
                let after=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();
                assert_eq!(after.lifecycle,"Connected","{profile}/{code} is a model refusal, not credential rejection");
                assert_eq!(after.credential_revision,before.credential_revision);assert_eq!(after.account_revision,before.account_revision);assert_eq!(after.healthy,before.healthy);
                assert!(f.connections.access(&id("connection"),None).is_ok());assert!(read(f.store.as_ref(),|s|s.router().cooldowns()).is_empty());assert_eq!(first.result.retry_at_ms,None);
                let second=produce_model(&f,"entitled-model","entitled-model","other-model-request").await.unwrap();assert_eq!(second.result.outcome,Some(AttemptEnd::Completed));
                let requests=server.requests.lock().unwrap();assert_eq!(requests.len(),2);assert!(String::from_utf8_lossy(&requests[1]).contains("entitled-model"));drop(requests);
                assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());server.finish();
            }
        }
    });
}

#[test]
fn genuine_http_401_and_403_still_reject_the_current_api_credential_without_replaying() {
    runtime().block_on(async {
        for status in [401, 403] {
            let mut f = Fx::new("OpenAiResponses", Script::aggregate(""), false);
            let server = LocalHttp::new(
                status,
                &[],
                r#"{"error":{"type":"authentication_error","message":"invalid credential"}}"#,
            );
            native(&mut f, &server.endpoint);
            other_model(&f);
            let result = f.produce(f.generation(vec![])).await.unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::Failed));
            assert!(
                result
                    .result
                    .faults
                    .iter()
                    .any(|fault| fault.starts_with("Authentication:"))
            );
            let connection = read(f.store.as_ref(), |s| {
                s.router().connection(&id("connection"))
            })
            .unwrap();
            assert_eq!(connection.lifecycle, "ReauthenticationRequired");
            assert_eq!(connection.healthy, Some(false));
            assert!(f.connections.access(&id("connection"), None).is_err());
            let blocked = produce_model(
                &f,
                "entitled-model",
                "entitled-model",
                "authentication-blocked-request",
            )
            .await;
            match blocked {
                Ok(value) => assert!(
                    matches!(value.accounting, DriveOutcome::Refused(_)),
                    "invalid credentials must not dispatch another model: {:?}",
                    value.accounting
                ),
                Err(StorageError::ScopeDenied) => {}
                Err(StorageError::StorageUnavailable {
                    cause: Unavailable::UnsupportedRuntime,
                    diagnostic,
                }) => assert_eq!(diagnostic, "current provider access unavailable"),
                Err(error) => panic!("unexpected credential admission failure: {error:?}"),
            }
            assert_eq!(server.requests.lock().unwrap().len(), 1);
            assert!(read(f.store.as_ref(), |s| s.router().cooldowns()).is_empty());
            server.finish();
        }
    });
}
struct RefreshOwner(AtomicUsize);
impl devworks_core::ports::http::HttpTransport for RefreshOwner {
    fn send(
        &self,
        request: &HttpRequest,
    ) -> Result<devworks_core::ports::http::HttpResponse, devworks_core::ports::http::HttpError>
    {
        assert!(request.url.contains("oauth/token"));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(devworks_core::ports::http::HttpResponse{status:200,body:br#"{"access_token":"rotated-fixture-access","refresh_token":"rotated-fixture-refresh","expires_in":3600,"account":{"uuid":"fixture-account"}}"#.to_vec()})
    }
}
#[test]
fn actual_http_oauth_401_refreshes_once_but_genuine_403_requires_reauthentication() {
    runtime().block_on(async {
        for status in [401, 403] {
            let refresh = Arc::new(RefreshOwner(AtomicUsize::new(0)));
            let mut f = Fx::with_access(
                "ClaudeSubscriptionMessages",
                Script::sse(""),
                false,
                refresh.clone(),
                Arc::new(SessionSecrets::new()),
            );
            let server = LocalHttp::new(
                status,
                &[],
                r#"{"error":{"type":"authentication_error","message":"credential rejected"}}"#,
            );
            native(&mut f, &server.endpoint);
            let result = f.produce(f.generation(vec![])).await.unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::Failed));
            assert!(
                result
                    .result
                    .faults
                    .iter()
                    .any(|fault| fault.starts_with("Authentication:"))
            );
            let connection = read(f.store.as_ref(), |s| {
                s.router().connection(&id("connection"))
            })
            .unwrap();
            if status == 401 {
                assert_eq!(connection.lifecycle, "Connected");
                assert_eq!(connection.credential_revision, 2);
                assert!(f.connections.access(&id("connection"), None).is_ok());
                assert_eq!(refresh.0.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(connection.lifecycle, "ReauthenticationRequired");
                assert_eq!(connection.credential_revision, 1);
                assert!(f.connections.access(&id("connection"), None).is_err());
                assert_eq!(refresh.0.load(Ordering::SeqCst), 0);
            }
            assert_eq!(connection.account_revision, 1);
            assert_eq!(
                server.requests.lock().unwrap().len(),
                1,
                "refresh never replays failed inference"
            );
            server.finish();
        }
    });
}
