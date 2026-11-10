use super::*;
use devworks_core::ports::{
    http::{HttpError, HttpResponse, HttpTransport},
    secrets::SecretError,
};
use std::time::Duration;
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    open: Mutex<bool>,
    changed: std::sync::Condvar,
}
impl Gate {
    fn wait(&self) {
        self.entered.notify_one();
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct RefreshHttp {
    gate: Option<Arc<Gate>>,
    calls: AtomicUsize,
}
impl HttpTransport for RefreshHttp {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        assert!(r.url.contains("oauth/token"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        Ok(HttpResponse {status:200,body:br#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600,"account":{"uuid":"fixture-account"}}"#.to_vec()})
    }
}
struct GatedSecrets {
    values: SessionSecrets,
    armed: AtomicBool,
    fail_after_put: AtomicBool,
    gate: Arc<Gate>,
}
impl SecretStore for GatedSecrets {
    fn name(&self) -> &'static str {
        "controlled session fixture"
    }
    fn put(&self, key: &SecretKey, value: &Secret) -> Result<(), SecretError> {
        if self.armed.load(Ordering::SeqCst) {
            self.gate.wait();
        }
        self.values.put(key, value)?;
        if self.fail_after_put.load(Ordering::SeqCst) {
            return Err(SecretError::Failed("external acknowledgement lost".into()));
        }
        Ok(())
    }
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        self.values.get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        self.values.delete(key)
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        self.values.list(namespace)
    }
}
fn due(f: &Fx, secrets: &dyn SecretStore) {
    use devworks_core::services::router::connections::auth::{Credential, Tokens};
    secrets
        .put(
            &SecretKey::new(f.store.info().store_id.0.as_str(), id("credential")),
            &Credential::OAuth(Tokens {
                access: Secret::new("expired-access"),
                refresh: Secret::new("fixture-refresh"),
                expires_at_ms: T0,
                account: Some("fixture-account".into()),
            })
            .to_secret(),
        )
        .unwrap();
}
const RESPONSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg\",\"model\":\"fixture-model\",\"usage\":{\"input_tokens\":10}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
#[test]
fn due_oauth_refresh_before_admission_requotes_exact_connection_and_sends_once() {
    runtime().block_on(async {
        let secrets = Arc::new(SessionSecrets::new());
        let http = Arc::new(RefreshHttp {
            gate: None,
            calls: AtomicUsize::new(0),
        });
        let f = Fx::with_access(
            "ClaudeSubscriptionMessages",
            Script::sse(RESPONSE),
            false,
            http.clone(),
            secrets.clone(),
        );
        due(&f, secrets.as_ref());
        let outcome = f.produce(f.generation(vec![])).await.unwrap();
        assert_eq!(outcome.result.outcome, Some(AttemptEnd::Completed));
        let connection = read(f.store.as_ref(), |s| {
            s.router().connection(&id("connection"))
        })
        .unwrap();
        assert_eq!(connection.credential_revision, 2);
        assert_eq!(connection.account_revision, 1);
        assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        let attempts = read(f.store.as_ref(), |s| {
            s.usage()
                .inference_attempts(&UsageAttemptFilter::default(), None, 64)
        });
        let captured = devworks_core::services::usage::accounting::snapshot(&attempts[0]).unwrap();
        assert_eq!(
            captured.quote.connection_revision,
            Some(connection.revision)
        );
        let op = read(f.store.as_ref(), |s| {
            s.operations()
                .operation(connection.refresh_operation_id.as_ref().unwrap())
        })
        .unwrap();
        assert_eq!(
            op.state,
            devworks_core::protocol::OperationState::ObservedComplete
        );
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
    });
}
#[test]
fn close_during_http_or_external_secret_write_preserves_exact_unknown_custody_and_fences_late_publication()
 {
    for secret_write in [false, true] {
        runtime().block_on(async {
            let gate = Arc::new(Gate::default());
            let secrets = Arc::new(GatedSecrets {
                values: SessionSecrets::new(),
                armed: AtomicBool::new(false),
                fail_after_put: AtomicBool::new(false),
                gate: gate.clone(),
            });
            let http = Arc::new(RefreshHttp {
                gate: (!secret_write).then(|| gate.clone()),
                calls: AtomicUsize::new(0),
            });
            let f = Arc::new(Fx::with_access(
                "ClaudeSubscriptionMessages",
                Script::sse(RESPONSE),
                false,
                http.clone(),
                secrets.clone(),
            ));
            due(&f, secrets.as_ref());
            secrets.armed.store(secret_write, Ordering::SeqCst);
            let request = f.generation(vec![]);
            let owner = f.clone();
            let task = tokio::spawn(async move { owner.produce(request).await });
            gate.entered.notified().await;
            let current = read(f.store.as_ref(), |s| {
                s.router().connection(&id("connection"))
            })
            .unwrap();
            let op_id = current.refresh_operation_id.clone().unwrap();
            let report = f.connections.close().unwrap();
            assert_eq!(report.unknown_operations, vec![op_id.clone()]);
            if secret_write {
                assert_eq!(report.unpublished_handles.len(), 1);
            }
            f.usage
                .drain_until(tokio::time::Instant::now() + Duration::from_millis(10))
                .await
                .unwrap();
            let before = read(f.store.as_ref(), |s| {
                s.router().connection(&id("connection"))
            })
            .unwrap();
            assert_eq!(before.credential_revision, 1);
            assert_eq!(before.lifecycle, "ReauthenticationRequired");
            let op = read(f.store.as_ref(), |s| s.operations().operation(&op_id)).unwrap();
            assert_eq!(
                op.state,
                devworks_core::protocol::OperationState::OutcomeUnknown
            );
            let allocated: Id =
                serde_json::from_value(op.payload["allocatedHandle"].clone()).unwrap();
            assert!(!allocated.as_str().is_empty());
            gate.release();
            assert!(task.await.unwrap().is_err());
            assert_eq!(
                read(f.store.as_ref(), |s| s
                    .router()
                    .connection(&id("connection")))
                .unwrap(),
                before
            );
            assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
            assert!(f.connections.access(&id("connection"), None).is_err());
            assert_eq!(http.calls.load(Ordering::SeqCst), 1);
            // A fresh owner sees the durable interrupted refresh and never retries the spent old token.
            let reopened = ConnectionService::new(
                f.h.ids.clone(),
                f.h.clock(),
                f.store.clone(),
                http.clone(),
                secrets,
                Arc::new(EntropyFixture),
            );
            assert!(reopened.access(&id("connection"), None).is_err());
            assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        });
    }
}
#[test]
fn bearer_401_refreshes_current_owner_once_without_replaying_failed_inference() {
    runtime().block_on(async {
        let secrets = Arc::new(SessionSecrets::new());
        let http = Arc::new(RefreshHttp {
            gate: None,
            calls: AtomicUsize::new(0),
        });
        let mut script =
            Script::aggregate(r#"{"error":{"type":"authentication_error","message":"expired"}}"#);
        script.status = 401;
        let f = Fx::with_access(
            "ClaudeSubscriptionMessages",
            script,
            false,
            http.clone(),
            secrets,
        );
        let result = f.produce(f.generation(vec![])).await.unwrap();
        assert_eq!(result.result.outcome, Some(AttemptEnd::Failed));
        assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
        let connection = read(f.store.as_ref(), |s| {
            s.router().connection(&id("connection"))
        })
        .unwrap();
        assert_eq!(connection.credential_revision, 2);
        assert_eq!(connection.account_revision, 1);
    });
}

#[test]
fn successful_remote_refresh_with_ambiguous_secret_write_never_retries_spent_old_token() {
    runtime().block_on(async {
        let gate = Arc::new(Gate::default());
        let secrets = Arc::new(GatedSecrets {
            values: SessionSecrets::new(),
            armed: AtomicBool::new(false),
            fail_after_put: AtomicBool::new(false),
            gate,
        });
        let http = Arc::new(RefreshHttp {
            gate: None,
            calls: AtomicUsize::new(0),
        });
        let f = Fx::with_access(
            "ClaudeSubscriptionMessages",
            Script::sse(RESPONSE),
            false,
            http.clone(),
            secrets.clone(),
        );
        due(&f, secrets.as_ref());
        secrets.fail_after_put.store(true, Ordering::SeqCst);
        assert!(f.produce(f.generation(vec![])).await.is_err());
        let current = read(f.store.as_ref(), |s| {
            s.router().connection(&id("connection"))
        })
        .unwrap();
        assert_eq!(current.lifecycle, "ReauthenticationRequired");
        assert_eq!(current.credential_revision, 1);
        let operation = read(f.store.as_ref(), |s| {
            s.operations()
                .operation(current.refresh_operation_id.as_ref().unwrap())
        })
        .unwrap();
        assert_eq!(
            operation.state,
            devworks_core::protocol::OperationState::OutcomeUnknown
        );
        let allocated: Id =
            serde_json::from_value(operation.payload["allocatedHandle"].clone()).unwrap();
        assert!(
            secrets
                .values
                .get(&SecretKey::new(
                    f.store.info().store_id.0.as_str(),
                    allocated
                ))
                .unwrap()
                .is_some(),
            "an external write may have happened even when acknowledgement fails"
        );
        assert!(f.connections.access(&id("connection"), None).is_err());
        assert_eq!(http.calls.load(Ordering::SeqCst), 1);
        assert!(f.http.lock().unwrap().requests.lock().unwrap().is_empty());
    });
}
#[test]
fn actual_native_continuation_survives_same_account_oauth_rotation() {
    runtime().block_on(async{
let secrets=Arc::new(SessionSecrets::new());let http=Arc::new(RefreshHttp{gate:None,calls:AtomicUsize::new(0)});let first="event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"actual-msg\",\"model\":\"fixture-model\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"actual retained thinking\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"actual-native-signature\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
let f=Fx::with_access("ClaudeSubscriptionMessages",Script::sse(first),false,http.clone(),secrets);assert_eq!(f.produce(f.generation(vec![])).await.unwrap().result.outcome,Some(AttemptEnd::Completed));let rows=f.transcript();let native=rows.iter().find(|r|r.kind=="NativeItem").expect("actual signed continuation");let binding:NativeBinding=serde_json::from_value(native.body["binding"].clone()).unwrap();let artifact:ArtifactRef=serde_json::from_value(native.body["artifact"].clone()).unwrap();
f.h.advance(86_400_000);f.http.lock().unwrap().body=VecDeque::from([RESPONSE.as_bytes().to_vec()]);let c=ProviderContext{version:1,instructions:vec![],instruction_stamps:vec![],skill_stamp:None,messages:vec![Message{role:MessageRole::Assistant,content:vec![ContentBlock::Native{binding:binding.clone(),artifact}]}],parameters:Default::default()};let mut request=f.request(InferenceInput::Generation{session_id:id("session"),context:f.bytes(&serde_json::to_vec(&c).unwrap()),context_epoch:Counter::new(0).unwrap(),tools:vec![],response_headroom_tokens:Revision::new(10).unwrap(),requires_images:false,requires_parallel_tools:false});request.request_id=id("after-ordinary-rotation");assert_eq!(f.produce(request).await.unwrap().result.outcome,Some(AttemptEnd::Completed));let current=read(f.store.as_ref(),|s|s.router().connection(&id("connection"))).unwrap();assert_eq!(current.credential_revision,2);assert_eq!(current.account_revision,binding.account_revision.get());assert_eq!(http.calls.load(Ordering::SeqCst),1);assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(),2);
});
}
