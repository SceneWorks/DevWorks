//! Provider connections end to end on the real SQLite store (DW-18): `router.connections` and
//! `router.configure` through the gateway and host authority, with receipts, the router reading
//! the same rows, credentials in a session-only secret store and every provider request answered
//! by a scripted in-memory transport. Nothing here contacts a provider or runs inference.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0017_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::http::{
    Entropy, HttpError, HttpRequest, HttpResponse, HttpTransport, Method, base64url,
};
use devworks_core::ports::secrets::{Secret, SecretKey, SecretStore, SessionSecrets};
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{ConnectionLifecycle, ConnectionRecord, RouteAvailability};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::router::InferenceRouter;
use devworks_core::services::router::connections::auth::Tokens;
use devworks_core::services::router::connections::profiles::{AuthEndpoints, risk_notice_sha256};
use devworks_core::services::router::connections::{
    AccessCredential, AccessError, ConnectionService, DISCOVERY_INTERVAL_MS,
    open_router_request_fields,
};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}

/// One scripted route: method, URL prefix and its queued (status, body, served) responses.
type Route = (Method, String, VecDeque<(u16, String, bool)>);
/// A scripted provider: responses per (method, URL prefix), in order (the last one repeats).
/// Status 0 is a transport failure.
#[derive(Default)]
struct FakeHttp {
    routes: Mutex<Vec<Route>>,
    seen: Mutex<Vec<HttpRequest>>,
    /// Runs once, while the first request to its URL prefix is in flight (a concurrent change).
    during: Mutex<Option<During>>,
}
type During = (String, Box<dyn FnOnce() + Send>);
/// A change to run while one handle is read.
type HandleHook = (Id, Box<dyn FnOnce() + Send>);
impl FakeHttp {
    fn during(&self, prefix: &str, f: impl FnOnce() + Send + 'static) {
        *self.during.lock().unwrap() = Some((prefix.to_owned(), Box::new(f)));
    }
    fn on(&self, method: Method, prefix: &str, status: u16, body: Value) {
        let mut routes = self.routes.lock().unwrap();
        let entry = (status, body.to_string(), false);
        if let Some(r) = routes
            .iter_mut()
            .find(|(m, p, _)| *m == method && p == prefix)
        {
            r.2.retain(|e| !e.2);
            r.2.push_back(entry);
        } else {
            routes.push((method, prefix.to_owned(), VecDeque::from([entry])));
        }
    }
    /// Replaces every scripted response of a route.
    fn reset(&self, method: Method, prefix: &str, status: u16, body: Value) {
        self.routes
            .lock()
            .unwrap()
            .retain(|(m, p, _)| !(*m == method && p == prefix));
        self.on(method, prefix, status, body);
    }
    fn requests(&self, prefix: &str) -> Vec<HttpRequest> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.starts_with(prefix))
            .cloned()
            .collect()
    }
}
impl HttpTransport for FakeHttp {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.seen.lock().unwrap().push(r.clone());
        let hook = {
            let mut during = self.during.lock().unwrap();
            match during.as_ref() {
                Some((prefix, _)) if r.url.starts_with(prefix.as_str()) => during.take(),
                _ => None,
            }
        };
        if let Some((_, f)) = hook {
            f();
        }
        let mut routes = self.routes.lock().unwrap();
        let Some((_, _, queue)) = routes
            .iter_mut()
            .filter(|(m, p, _)| *m == r.method && r.url.starts_with(p.as_str()))
            .max_by_key(|(_, p, _)| p.len())
        else {
            return Err(HttpError::Unreachable("nothing listens there".into()));
        };
        let (status, body) = if queue.len() > 1 {
            let (s, b, _) = queue.pop_front().unwrap();
            (s, b)
        } else {
            // The last response repeats until a newer one is scripted.
            let front = queue.front_mut().unwrap();
            front.2 = true;
            (front.0, front.1.clone())
        };
        if status == 0 {
            return Err(HttpError::Unreachable("scripted failure".into()));
        }
        Ok(HttpResponse {
            status,
            body: body.into_bytes(),
        })
    }
}

/// The session store, with a change that runs once while a `get` of one handle is in flight
/// (a concurrent rotation or replacement).
struct Hooked {
    inner: Arc<SessionSecrets>,
    during_get: Mutex<Option<HandleHook>>,
}
impl Hooked {
    fn during_get(&self, handle: &Id, f: impl FnOnce() + Send + 'static) {
        *self.during_get.lock().unwrap() = Some((handle.clone(), Box::new(f)));
    }
}
impl SecretStore for Hooked {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn put(
        &self,
        key: &SecretKey,
        secret: &Secret,
    ) -> Result<(), devworks_core::ports::secrets::SecretError> {
        self.inner.put(key, secret)
    }
    fn get(
        &self,
        key: &SecretKey,
    ) -> Result<Option<Secret>, devworks_core::ports::secrets::SecretError> {
        let hook = {
            let mut during = self.during_get.lock().unwrap();
            match during.as_ref() {
                Some((h, _)) if *h == key.handle => during.take(),
                _ => None,
            }
        };
        if let Some((_, f)) = hook {
            f();
        }
        self.inner.get(key)
    }
    fn delete(&self, key: &SecretKey) -> Result<bool, devworks_core::ports::secrets::SecretError> {
        self.inner.delete(key)
    }
    fn list(&self, namespace: &str) -> Result<Vec<Id>, devworks_core::ports::secrets::SecretError> {
        self.inner.list(namespace)
    }
}

/// Deterministic "randomness" (tests only).
struct Counting(AtomicU8);
impl Entropy for Counting {
    fn fill(&self, buf: &mut [u8]) {
        for b in buf {
            *b = self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

const ANTHROPIC: &str = "https://api.anthropic.test";
const CLAUDE_TOKEN: &str = "https://console.anthropic.test/v1/oauth/token";
const CODEX_ISSUER: &str = "https://auth.openai.test";
const CODEX_API: &str = "https://chatgpt.test/backend-api/codex";
const OPENROUTER: &str = "https://openrouter.test/api/v1";

struct Cx {
    h: Harness,
    store: Arc<SqliteStore>,
    svc: Arc<ConnectionService>,
    cases: UseCases,
    http: Arc<FakeHttp>,
    secrets: Arc<SessionSecrets>,
    hooked: Arc<Hooked>,
    n: AtomicU64,
}

impl Cx {
    fn new() -> Self {
        let cx = Self::open(
            Harness::new(),
            Arc::new(FakeHttp::default()),
            Arc::new(SessionSecrets::new()),
        );
        cx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("user-b", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let by = cx.user();
        cx.write(move |tx| {
            auth::bind_principal(
                tx,
                &by,
                NewPrincipalBinding {
                    actor: actor("agent-a"),
                    kind: PrincipalKind::Agent,
                    role: Some(AgentRole::Worker),
                    project_id: None,
                    tool_families: None,
                    bound_by: actor("user-a"),
                },
                None,
            )
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        });
        cx
    }
    fn open(h: Harness, http: Arc<FakeHttp>, secrets: Arc<SessionSecrets>) -> Self {
        let store = Arc::new(h.store());
        let hooked = Arc::new(Hooked {
            inner: secrets.clone(),
            during_get: Mutex::new(None),
        });
        let svc = Arc::new(
            ConnectionService::new(
                h.ids.clone(),
                h.clock.clone(),
                store.clone(),
                http.clone(),
                hooked.clone(),
                Arc::new(Counting(AtomicU8::new(0))),
            )
            .with_endpoints(AuthEndpoints {
                claude_authorize_url: "https://claude.test/oauth/authorize".into(),
                claude_token_url: CLAUDE_TOKEN.into(),
                codex_issuer: CODEX_ISSUER.into(),
                ..AuthEndpoints::default()
            }),
        );
        let router = Arc::new(InferenceRouter::new(
            h.ids.clone(),
            h.clock.clone(),
            svc.clone(),
        ));
        let cases = UseCases::new()
            .with_settings(Arc::new(
                devworks_core::settings::service::SettingsService {
                    roles: Arc::new(devworks_core::authority::HostRoles {
                        store: store.clone(),
                    }),
                    paths: Arc::new(NoPaths),
                    ids: h.ids.clone(),
                },
            ))
            .with_router(router)
            .with_connections(svc.clone());
        Self {
            h,
            store,
            svc,
            cases,
            http,
            secrets,
            hooked,
            n: AtomicU64::new(0),
        }
    }
    /// A process restart over the same data root, secret store and provider.
    fn restart(self) -> Self {
        let Cx {
            h,
            store,
            svc,
            cases,
            http,
            secrets,
            hooked,
            n,
        } = self;
        drop((cases, svc, store, hooked));
        let cx = Self::open(h, http, secrets);
        cx.n.store(n.load(Ordering::SeqCst), Ordering::SeqCst);
        cx
    }
    fn advance(&self, ms: i64) {
        self.h.clock.0.fetch_add(ms, Ordering::SeqCst);
    }
    fn now(&self) -> i64 {
        self.h.clock.0.load(Ordering::SeqCst)
    }
    fn ctx(&self, who: &str, kind: PrincipalKind) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            actor(who),
            kind,
            id("invocation-1"),
            id("ui-main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human)
    }
    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> T {
        run_write(&*self.store, WriteOptions::default(), f).expect("write")
    }
    fn read<T>(&self, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
        read_snapshot(&*self.store, &ReadOptions::default(), f).expect("read")
    }
    fn authority(&self) -> HostAuthority<'_> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.h.clock,
            protected_branches: &[],
        }
    }
    fn command_keyed(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let mut frame =
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .command(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn configure_as(&self, ctx: &InvocationContext, action: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_keyed(
            ctx,
            "router.configure",
            &key,
            json!({"action": action}),
            Some(2),
        )
    }
    fn configure(&self, action: Value) -> DomainCommandReply {
        self.configure_as(&self.user(), action)
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainQueryReply {
        body["kind"] = json!(op);
        let mut frame =
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-1", "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
        let authority = self.authority();
        let gateway = Gateway {
            authority: &authority,
            store: &*self.store,
            ids: &*self.h.ids,
        };
        self.cases
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn connections_as(&self, ctx: &InvocationContext, revision: u32) -> Vec<ConnectionRecord> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut page = json!({"limit": 2});
            if let Some(c) = &cursor {
                page["cursor"] = json!(c);
            }
            match result(self.query(
                ctx,
                "router.connections",
                json!({"page": page}),
                Some(revision),
            )) {
                OperationResult::RouterConnections { value } => {
                    out.extend(value.items.0);
                    match value.next_cursor {
                        Some(c) => cursor = Some(c.0),
                        None => return out,
                    }
                }
                other => panic!("{other:?}"),
            }
        }
    }
    fn connection(&self, connection: &str) -> ConnectionRecord {
        self.connections_as(&self.user(), 2)
            .into_iter()
            .find(|c| c.id.as_str() == connection)
            .expect("listed")
    }
    fn row(&self, connection: &str) -> StoredConnection {
        let c = id(connection);
        self.read(move |r| r.router().connection(&c))
            .expect("stored")
    }
    fn availability(&self, model: &str) -> RouteAvailability {
        match result(self.query(
            &self.user(),
            "router.availability",
            json!({"intent": {"kind": "Model", "modelId": model}, "effort": "Default",
                "requiredCapabilities": []}),
            Some(2),
        )) {
            OperationResult::RouterAvailability { value } => value,
            other => panic!("{other:?}"),
        }
    }
    /// Creates a connection with a caller-chosen ID.
    fn add(&self, connection: &str, profile: &str, endpoint: &str, key: Option<&Id>) -> i64 {
        let mut action = json!({"kind": "Connection", "connectionId": connection,
            "name": connection, "profile": profile, "endpoint": endpoint});
        if let Some(k) = key {
            action["credentialRef"] = json!(k.as_str());
        }
        summary(self.configure(action)).1
    }
    fn stage(&self, key: &str) -> Id {
        self.svc
            .stage_api_key(&actor("user-a"), Secret::new(key))
            .expect("staged")
    }
    fn secret(&self, handle: &Id) -> Option<Secret> {
        let ns = self.store.info().store_id.0.as_str().to_owned();
        self.secrets
            .get(&SecretKey::new(ns, handle.clone()))
            .unwrap()
    }
    /// Every text value of every table, to prove a secret never reached SQLite.
    fn database_text(&self) -> String {
        self.store
            .with_writer_connection(|c| {
                let tables: Vec<String> = super::super::conn::db(
                    c.prepare("SELECT name FROM sqlite_schema WHERE type='table'")
                        .and_then(|mut s| {
                            s.query_map([], |r| r.get::<_, String>(0))?
                                .collect::<rusqlite::Result<Vec<_>>>()
                        }),
                )?;
                let mut out = String::new();
                for t in tables {
                    let mut st =
                        super::super::conn::db(c.prepare(&format!("SELECT * FROM \"{t}\"")))?;
                    let n = st.column_count();
                    let rows = super::super::conn::db(
                        st.query_map([], |r| {
                            let mut line = String::new();
                            for i in 0..n {
                                if let Ok(rusqlite::types::ValueRef::Text(b)) = r.get_ref(i) {
                                    line.push_str(&String::from_utf8_lossy(b));
                                    line.push('|');
                                }
                            }
                            Ok(line)
                        })
                        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()),
                    )?;
                    out.push_str(&rows.join("\n"));
                }
                Ok(out)
            })
            .unwrap()
    }
}

struct NoPaths;
impl devworks_core::settings::service::PathProbe for NoPaths {
    fn probe(&self, _: &str) -> devworks_core::settings::service::PathHealth {
        devworks_core::settings::service::PathHealth::Missing
    }
}

fn recorded(reply: DomainCommandReply) -> DomainDecision {
    match reply {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
/// `(id, revision, applied)` of a `router.configure` summary.
fn summary(reply: DomainCommandReply) -> (String, i64, bool) {
    match recorded(reply) {
        DomainDecision::Applied {
            result: OperationResult::RouterConfigure { value },
        } => (value.id.as_str().to_owned(), value.revision.get(), true),
        DomainDecision::NoChange {
            result: OperationResult::RouterConfigure { value },
        } => (value.id.as_str().to_owned(), value.revision.get(), false),
        other => panic!("expected a configure summary, got {other:?}"),
    }
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply) {
        DomainDecision::Refused { fault } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn result(reply: DomainQueryReply) -> OperationResult {
    match reply {
        DomainQueryReply::Result { result, .. } => result,
        other => panic!("expected a result, got {other:?}"),
    }
}
fn codes(a: &RouteAvailability) -> Vec<&str> {
    a.blocked_reason_codes
        .0
        .iter()
        .map(|c| c.0.as_str())
        .collect()
}
fn lifecycle(c: &ConnectionRecord) -> ConnectionLifecycle {
    c.lifecycle
        .clone()
        .expect("revision 2 carries the lifecycle")
}
fn is(l: ConnectionLifecycle, want: &str) -> bool {
    serde_json::to_value(l).unwrap() == json!(want)
}
fn jwt(claims: Value) -> String {
    format!("e30.{}.sig", base64url(claims.to_string().as_bytes()))
}
fn header<'a>(r: &'a HttpRequest, name: &str) -> Option<&'a str> {
    r.header_value(name)
}
fn body_json(r: &HttpRequest) -> Value {
    serde_json::from_slice(r.body.as_deref().unwrap()).unwrap()
}
fn body_form(r: &HttpRequest) -> String {
    String::from_utf8(r.body.clone().unwrap()).unwrap()
}

// ---------------------------------------------------------------------------------------------

#[test]
fn api_key_connections_publish_test_replace_and_remove_their_credential() {
    let cx = Cx::new();
    let key = cx.stage("sk-ant-api-first");
    cx.add("anthropic", "AnthropicMessages", ANTHROPIC, Some(&key));
    let c = cx.connection("anthropic");
    assert!(is(lifecycle(&c), "Connected"));
    assert_eq!(
        c.credential_ref.as_ref().map(Id::as_str),
        Some(key.as_str())
    );
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(1));
    assert_eq!(
        cx.secret(&key),
        Some(Secret::new(r#"{"key":"sk-ant-api-first","kind":"apiKey"}"#,))
    );

    // A catalog model of this profile, its context window unknown until discovered.
    let model = json!({"id": "opus-api", "displayName": "Opus (API)",
        "endpointProfile": "AnthropicMessages", "upstreamModelId": "claude-opus-5-5",
        "enabled": true, "tierMemberships": [], "efforts": ["Low", "High"],
        "imageInput": true, "tools": true, "revision": "1"});
    assert!(summary(cx.configure(json!({"kind": "Model", "model": model}))).2);

    // The connection test is an authenticated model list, never an inference.
    cx.http.on(
        Method::Get,
        &format!("{ANTHROPIC}/v1/models"),
        200,
        json!({
        "data": [{"id": "claude-opus-5-5", "max_input_tokens": 1000000},
                 {"id": "claude-haiku-5-5"}],
        "has_more": false}),
    );
    let (_, _, changed) = summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "anthropic",
        "action": "Inspect"})),
    );
    assert!(changed);
    let sent = cx.http.requests(&format!("{ANTHROPIC}/v1/models"));
    assert_eq!(header(&sent[0], "x-api-key"), Some("sk-ant-api-first"));
    assert_eq!(header(&sent[0], "anthropic-version"), Some("2023-06-01"));
    // Never the subscription identity or beta on the API-key profile.
    assert_eq!(header(&sent[0], "anthropic-beta"), None);
    assert_eq!(header(&sent[0], "authorization"), None);
    let c = cx.connection("anthropic");
    assert_eq!(c.healthy, Some(true));
    assert_eq!(c.discovered_model_count.as_ref().map(Counter::get), Some(2));
    let m = cx
        .read(|r| r.router().route_model(&id("opus-api")))
        .unwrap();
    assert_eq!(
        m.context_tokens,
        Some(1_000_000),
        "discovered context window applied"
    );
    assert!(cx.availability("opus-api").available);

    // Replacement: a new handle, the next credential revision, the old secret deleted.
    let revision = c.revision.get();
    let second = cx.stage("sk-ant-api-second");
    assert!(
        summary(
            cx.configure(json!({"kind": "Connection", "connectionId": "anthropic",
        "expectedRevision": revision.to_string(), "name": "Anthropic",
        "profile": "AnthropicMessages", "endpoint": ANTHROPIC,
        "credentialRef": second.as_str()}))
        )
        .2
    );
    let c = cx.connection("anthropic");
    assert_eq!(
        c.credential_ref.as_ref().map(Id::as_str),
        Some(second.as_str())
    );
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(2));
    assert_eq!(cx.secret(&key), None, "the replaced key is deleted");
    // A stale revision is a conflict, not a silent overwrite.
    let stale = refused(
        cx.configure(json!({"kind": "Connection", "connectionId": "anthropic",
        "expectedRevision": revision.to_string(), "name": "x", "profile": "AnthropicMessages",
        "endpoint": ANTHROPIC})),
    );
    assert_eq!(stale.code, DomainErrorCode::RevisionConflict);

    // Removal of the key keeps the connection; the router then needs credentials.
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "anthropic",
        "action": "Disconnect"}))
        )
        .2
    );
    let c = cx.connection("anthropic");
    assert!(is(lifecycle(&c), "Disconnected"));
    assert_eq!(c.credential_ref, None);
    assert_eq!(cx.secret(&second), None);
    assert!(codes(&cx.availability("opus-api")).contains(&"CredentialsRequired"));

    // The bound-model guard: a connection a model is bound to cannot be removed.
    let c = cx.connection("anthropic");
    let bound = json!({"id": "opus-api", "displayName": "Opus (API)",
        "endpointProfile": "AnthropicMessages", "upstreamModelId": "claude-opus-5-5",
        "enabled": true, "tierMemberships": [], "efforts": ["Low", "High"],
        "imageInput": true, "tools": true, "revision": "1", "connectionId": "anthropic",
        "contextTokens": "1000000"});
    let m = cx
        .read(|r| r.router().route_model(&id("opus-api")))
        .unwrap();
    assert!(
        summary(cx.configure(json!({"kind": "Model", "model": bound,
        "expectedRevision": m.revision.to_string()})))
        .2
    );
    let in_use = refused(cx.configure(json!({"kind": "RemoveConnection",
        "connectionId": "anthropic", "expectedRevision": c.revision.get().to_string()})));
    assert_eq!(in_use.code, DomainErrorCode::Conflict);

    // No key ever reached SQLite (rows, receipts, history).
    let text = cx.database_text();
    assert!(!text.contains("sk-ant-api-first") && !text.contains("sk-ant-api-second"));
}

#[test]
fn staged_keys_are_bound_to_their_principal_and_only_publish_to_key_profiles() {
    let cx = Cx::new();
    let key = cx.stage("sk-or-key");
    // Another human cannot publish a key they did not stage.
    let other = cx.ctx("user-b", PrincipalKind::Human);
    let f = refused(cx.configure_as(&other, json!({"kind": "Connection", "connectionId": "or",
        "name": "OpenRouter", "profile": "OpenRouterChat", "endpoint": "https://openrouter.test/api/v1",
        "credentialRef": key.as_str()})));
    assert_eq!(f.code, DomainErrorCode::NotFound);
    // A subscription profile never takes a key: its credential comes only from its sign-in.
    let f = refused(
        cx.configure(json!({"kind": "Connection", "connectionId": "codex",
        "name": "Codex", "profile": "CodexSubscriptionResponses", "endpoint": CODEX_API,
        "credentialRef": key.as_str()})),
    );
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    // An agent cannot configure connections at all (human configuration).
    let agent = cx.ctx("agent-a", PrincipalKind::Agent);
    let denied = cx.configure_as(
        &agent,
        json!({"kind": "Connection", "connectionId": "or",
        "name": "OpenRouter", "profile": "OpenRouterChat",
        "endpoint": "https://openrouter.test/api/v1"}),
    );
    assert!(!matches!(denied, CommandReply::Recorded { receipt, .. }
        if matches!(receipt.decision, DomainDecision::Applied { .. })));
    // Plain HTTP only for explicitly configured compatible endpoints.
    let f = refused(cx.configure(json!({"kind": "Connection", "connectionId": "or",
        "name": "OpenRouter", "profile": "OpenRouterChat", "endpoint": "http://openrouter.test/api/v1"})));
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    // The owner can.
    cx.add(
        "or",
        "OpenRouterChat",
        "https://openrouter.test/api/v1",
        Some(&key),
    );
    assert!(cx.secret(&key).is_some());
    // Agents may read connections, never a credential value.
    let listed = cx.connections_as(&agent, 2);
    assert_eq!(listed.len(), 1);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("sk-or-key")
    );
}

#[test]
fn a_claude_subscription_is_disabled_until_the_exact_notice_is_accepted_and_signs_in_with_pkce() {
    let cx = Cx::new();
    cx.add("claude", "ClaudeSubscriptionMessages", ANTHROPIC, None);
    let c = cx.connection("claude");
    assert!(is(lifecycle(&c), "Disabled"));
    assert!(!c.subscription_risk_accepted);
    assert!(codes(&cx.availability("opus")).contains(&"ConnectionDisabled"));
    // No sign-in before acceptance.
    let f = refused(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "BeginSignIn"})),
    );
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    // Only the exact current notice can be accepted.
    let f = refused(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": "0".repeat(64),
        "accepted": true})),
    );
    assert_eq!(f.code, DomainErrorCode::SourceChanged);
    assert!(
        summary(
            cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true}))
        )
        .2
    );
    let c = cx.connection("claude");
    assert!(c.subscription_risk_accepted);
    assert!(is(lifecycle(&c), "Disconnected"));

    // Browser sign-in: the challenge is shown only to the human who began it.
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "BeginSignIn"}))
        )
        .2
    );
    let c = cx.connection("claude");
    assert!(is(lifecycle(&c), "Authorizing"));
    let challenge = c.sign_in.clone().expect("challenge");
    let url = challenge.authorize_url.expect("authorize URL").0;
    assert!(url.starts_with("https://claude.test/oauth/authorize?code=true&client_id="));
    assert!(url.contains("code_challenge_method=S256"));
    let other = cx.ctx("user-b", PrincipalKind::Human);
    assert!(cx.connections_as(&other, 2)[0].sign_in.is_none());
    let state = url.split("state=").nth(1).unwrap().to_owned();
    let flow = challenge.flow_id.clone();

    // A pasted value with another state is refused before any exchange.
    cx.svc
        .submit_authorization_code(&actor("user-a"), &flow, Secret::new("the-code#forged"))
        .unwrap();
    let f = refused(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "CompleteSignIn", "flowId": flow.as_str()})),
    );
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    assert!(cx.http.requests(CLAUDE_TOKEN).is_empty());
    // Another principal cannot paste into this sign-in.
    assert!(
        cx.svc
            .submit_authorization_code(&actor("user-b"), &flow, Secret::new("x#y"))
            .is_err()
    );

    cx.http.on(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-1",
        "refresh_token": "rt-1", "expires_in": 3600, "account": {"uuid": "account-1"}}),
    );
    cx.svc
        .submit_authorization_code(
            &actor("user-a"),
            &flow,
            Secret::new(format!("the-code#{state}")),
        )
        .unwrap();
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "CompleteSignIn", "flowId": flow.as_str()}))
        )
        .2
    );
    let exchange = &cx.http.requests(CLAUDE_TOKEN)[0];
    let body = body_json(exchange);
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "the-code");
    // The verifier the exchange sends is the one whose S256 challenge the browser saw.
    let verifier = body["code_verifier"].as_str().unwrap();
    use sha2::Digest;
    let challenge = base64url(sha2::Sha256::digest(verifier.as_bytes()).as_slice());
    assert!(url.contains(&format!("code_challenge={challenge}")));
    assert_eq!(header(exchange, "user-agent"), Some("simple-router/1.0"));
    let c = cx.connection("claude");
    assert!(is(lifecycle(&c), "Connected"), "{c:?}");
    assert!(
        c.subscription_risk_accepted,
        "the first account binds the acceptance"
    );
    assert!(c.sign_in.is_none());
    assert!(cx.availability("opus").available);
    let text = cx.database_text();
    assert!(!text.contains("at-1") && !text.contains("rt-1") && !text.contains("the-code"));

    // Token refresh: a token near expiry is refreshed once and the rotation persisted.
    cx.http.on(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-2",
        "refresh_token": "rt-2", "expires_in": 3600, "account": {"uuid": "account-1"}}),
    );
    cx.advance(3_600_000);
    let before = cx.row("claude");
    let access = cx.svc.access(&id("claude"), None).unwrap();
    assert!(
        matches!(&access.credential, AccessCredential::Bearer { token, .. } if token.expose() == "at-2")
    );
    let refresh = cx.http.requests(CLAUDE_TOKEN).pop().unwrap();
    assert_eq!(body_json(&refresh)["grant_type"], "refresh_token");
    assert_eq!(body_json(&refresh)["refresh_token"], "rt-1");
    let after = cx.row("claude");
    assert_eq!(after.credential_revision, before.credential_revision + 1);
    assert_eq!(
        cx.secret(before.credential_ref.as_ref().unwrap()),
        None,
        "the rotated handle is deleted"
    );
    // A rejected refresh requires reauthentication of this connection only.
    cx.http.reset(
        Method::Post,
        CLAUDE_TOKEN,
        400,
        json!({"error": "invalid_grant"}),
    );
    cx.advance(3_600_000);
    assert!(matches!(
        cx.svc.access(&id("claude"), None),
        Err(AccessError::ReauthenticationRequired(_))
    ));
    assert!(is(
        lifecycle(&cx.connection("claude")),
        "ReauthenticationRequired"
    ));
    assert!(codes(&cx.availability("opus")).contains(&"CredentialsRequired"));

    // Another account revokes the acceptance.
    let c = cx.connection("claude");
    let _ = c;
    let flow = {
        let reply = cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
            "action": "BeginSignIn"}));
        assert!(summary(reply).2);
        cx.connection("claude").sign_in.unwrap()
    };
    let state = flow
        .authorize_url
        .unwrap()
        .0
        .split("state=")
        .nth(1)
        .unwrap()
        .to_owned();
    cx.http.reset(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-3",
        "refresh_token": "rt-3", "expires_in": 3600, "account": {"uuid": "account-2"}}),
    );
    cx.svc
        .submit_authorization_code(
            &actor("user-a"),
            &flow.flow_id,
            Secret::new(format!("c2#{state}")),
        )
        .unwrap();
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "CompleteSignIn", "flowId": flow.flow_id.as_str()}))
        )
        .2
    );
    let c = cx.connection("claude");
    assert!(
        !c.subscription_risk_accepted,
        "a new account needs a new acceptance"
    );
    assert!(is(lifecycle(&c), "Disabled"));
}

#[test]
fn risk_acceptance_is_revoked_by_endpoint_changes_and_restore() {
    let cx = Cx::new();
    cx.add("claude", "ClaudeSubscriptionMessages", ANTHROPIC, None);
    let c = cx.connection("claude");
    summary(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true})),
    );
    let c = cx.connection("claude");
    assert!(c.subscription_risk_accepted);
    // Another endpoint is not what was accepted.
    summary(
        cx.configure(json!({"kind": "Connection", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "name": "Claude",
        "profile": "ClaudeSubscriptionMessages", "endpoint": "https://proxy.example.test"})),
    );
    let c = cx.connection("claude");
    assert!(!c.subscription_risk_accepted);
    assert!(is(lifecycle(&c), "Disabled"));
    // Accept again, then restore the workspace: the acceptance does not survive a restore.
    summary(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true})),
    );
    assert!(cx.connection("claude").subscription_risk_accepted);
    // A credential published before the restore is not part of the backup either.
    let key = cx.stage("sk-before-restore");
    cx.add(
        "or",
        "OpenRouterChat",
        "https://openrouter.test/api/v1",
        Some(&key),
    );
    assert!(cx.svc.access(&id("or"), None).is_ok());
    cx.store
        .with_writer_connection(|c| {
            super::super::conn::db(c.execute(
                "UPDATE store_meta SET restore_epoch = restore_epoch + 1",
                [],
            ))
        })
        .unwrap();
    let cx = cx.restart();
    let c = cx.connection("claude");
    assert!(
        !c.subscription_risk_accepted,
        "a restore revokes the acceptance"
    );
    assert!(is(
        lifecycle(&cx.connection("or")),
        "ReauthenticationRequired"
    ));
    assert!(matches!(
        cx.svc.access(&id("or"), None),
        Err(AccessError::ReauthenticationRequired(_))
    ));
    assert!(codes(&cx.availability("openrouter-auto")).contains(&"CredentialsRequired"));
}

#[test]
fn codex_device_sign_in_polls_at_its_interval_and_discovers_the_account_models() {
    let cx = Cx::new();
    cx.add("codex", "CodexSubscriptionResponses", CODEX_API, None);
    cx.http.on(
        Method::Post,
        &format!("{CODEX_ISSUER}/api/accounts/deviceauth/usercode"),
        200,
        json!({"device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "5"}),
    );
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "codex",
        "action": "BeginSignIn"}))
        )
        .2
    );
    let c = cx.connection("codex");
    assert!(is(lifecycle(&c), "Authorizing"));
    let ch = c.sign_in.unwrap();
    assert_eq!(
        ch.user_code.as_ref().map(|l| l.0.as_str()),
        Some("ABCD-EFGH")
    );
    assert_eq!(
        ch.verification_url.as_ref().map(|u| u.0.as_str()),
        Some(&*format!("{CODEX_ISSUER}/codex/device"))
    );
    let poll = format!("{CODEX_ISSUER}/api/accounts/deviceauth/token");
    // Not polled before its interval.
    cx.svc.tick();
    assert!(cx.http.requests(&poll).is_empty());
    // Pending (403) keeps waiting.
    cx.http.on(Method::Post, &poll, 403, json!({}));
    cx.advance(5_000);
    cx.svc.tick();
    assert_eq!(cx.http.requests(&poll).len(), 1);
    assert!(is(lifecycle(&cx.connection("codex")), "Authorizing"));
    // Approved: exchange, publish and discover.
    let id_token = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-9"}}));
    cx.http.reset(
        Method::Post,
        &poll,
        200,
        json!({"authorization_code": "auth-1", "code_verifier": "ver-1"}),
    );
    cx.http.on(Method::Post, &format!("{CODEX_ISSUER}/oauth/token"), 200,
        json!({"access_token": "codex-at", "refresh_token": "codex-rt", "id_token": id_token, "expires_in": 3600}));
    cx.http.on(
        Method::Get,
        &format!("{CODEX_API}/models"),
        200,
        json!({"models": [{"slug": "gpt-6-luna"}, {"slug": "gpt-6.1-sol"}]}),
    );
    cx.advance(5_000);
    cx.svc.tick();
    let exchange = &cx.http.requests(&format!("{CODEX_ISSUER}/oauth/token"))[0];
    let form = body_form(exchange);
    assert!(form.contains("grant_type=authorization_code") && form.contains("code_verifier=ver-1"));
    assert!(form.contains("redirect_uri=https%3A%2F%2Fauth.openai.test%2Fdeviceauth%2Fcallback"));
    let list = &cx.http.requests(&format!("{CODEX_API}/models"))[0];
    assert!(list.url.ends_with("/models?client_version=99.0.0"));
    assert_eq!(header(list, "chatgpt-account-id"), Some("acct-9"));
    assert_eq!(header(list, "originator"), Some("codex_cli_rs"));
    assert_eq!(header(list, "authorization"), Some("Bearer codex-at"));
    let c = cx.connection("codex");
    assert!(is(lifecycle(&c), "Connected"), "{c:?}");
    assert_eq!(c.discovered_model_ids.as_ref().map(|i| i.0.len()), Some(2));
    assert!(cx.availability("luna").available);
    // A discovered list that does not name a model makes it ineligible (never silently routed).
    assert!(codes(&cx.availability("astra")).contains(&"NotDiscovered"));
    // History records the sign-in without secrets.
    assert!(!cx.database_text().contains("codex-rt"));
}

#[test]
fn a_replayed_sign_in_request_starts_nothing_twice() {
    let cx = Cx::new();
    cx.add("codex", "CodexSubscriptionResponses", CODEX_API, None);
    let start = format!("{CODEX_ISSUER}/api/accounts/deviceauth/usercode");
    cx.http.on(
        Method::Post,
        &start,
        200,
        json!({"device_auth_id": "dev-1", "user_code": "ONE", "interval": 5}),
    );
    let body = json!({"action": {"kind": "SignIn", "connectionId": "codex",
        "action": "BeginSignIn"}});
    let first = cx.command_keyed(
        &cx.user(),
        "router.configure",
        "begin-1",
        body.clone(),
        Some(2),
    );
    assert!(summary(first).2);
    let flow = cx.connection("codex").sign_in.unwrap().flow_id;
    // The same request key replays its receipt: no second device code, the same open flow.
    let again = cx.command_keyed(&cx.user(), "router.configure", "begin-1", body, Some(2));
    assert!(summary(again).2);
    assert_eq!(cx.http.requests(&start).len(), 1);
    assert_eq!(cx.connection("codex").sign_in.unwrap().flow_id, flow);
}

#[test]
fn a_new_endpoint_origin_never_receives_the_old_key() {
    let cx = Cx::new();
    let key = cx.stage("sk-compat");
    cx.add(
        "vllm",
        "CompatibleChat",
        "https://models.example.test/v1",
        Some(&key),
    );
    let c = cx.connection("vllm");
    // Same origin, another path: the key stays.
    summary(
        cx.configure(json!({"kind": "Connection", "connectionId": "vllm",
        "expectedRevision": c.revision.get().to_string(), "name": "vLLM",
        "profile": "CompatibleChat", "endpoint": "https://models.example.test/v2"})),
    );
    let c = cx.connection("vllm");
    assert_eq!(c.credential_ref.as_ref(), Some(&key));
    // Another host: the key is dropped (and deleted) unless the same action supplies one.
    summary(
        cx.configure(json!({"kind": "Connection", "connectionId": "vllm",
        "expectedRevision": c.revision.get().to_string(), "name": "vLLM",
        "profile": "CompatibleChat", "endpoint": "https://elsewhere.example.test/v1"})),
    );
    let c = cx.connection("vllm");
    assert_eq!(c.credential_ref, None);
    assert_eq!(cx.secret(&key), None);
    cx.http.on(
        Method::Get,
        "https://elsewhere.example.test/v1/models",
        200,
        json!({"data": []}),
    );
    summary(cx.configure(json!({"kind": "SignIn", "connectionId": "vllm", "action": "Inspect"})));
    let sent = cx.http.requests("https://elsewhere.example.test");
    assert!(sent.iter().all(|r| header(r, "authorization").is_none()));
}

#[test]
fn a_denied_device_code_ends_the_sign_in_with_its_reason() {
    let cx = Cx::new();
    cx.add("codex", "CodexSubscriptionResponses", CODEX_API, None);
    cx.http.on(
        Method::Post,
        &format!("{CODEX_ISSUER}/api/accounts/deviceauth/usercode"),
        200,
        json!({"device_auth_id": "dev-1", "user_code": "CODE", "interval": 1}),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "codex", "action": "BeginSignIn"})),
    );
    cx.http.on(
        Method::Post,
        &format!("{CODEX_ISSUER}/api/accounts/deviceauth/token"),
        400,
        json!({}),
    );
    cx.advance(1_000);
    cx.svc.tick();
    let c = cx.connection("codex");
    assert!(is(lifecycle(&c), "Disconnected"));
    assert!(c.state_reason.unwrap().0.contains("denied or expired"));
    assert!(c.sign_in.is_none());
    // An unreachable issuer at the start is an honest external failure, not a sign-in.
    cx.http.reset(
        Method::Post,
        &format!("{CODEX_ISSUER}/api/accounts/deviceauth/usercode"),
        0,
        json!({}),
    );
    let f = refused(
        cx.configure(json!({"kind": "SignIn", "connectionId": "codex", "action": "BeginSignIn"})),
    );
    assert_eq!(f.code, DomainErrorCode::ExternalFailure);
}

#[test]
fn compatible_endpoints_need_no_key_and_bound_models_route_only_to_their_endpoint() {
    let cx = Cx::new();
    cx.add(
        "ollama",
        "CompatibleChat",
        "http://localhost:11434/v1",
        None,
    );
    cx.add(
        "lmstudio",
        "CompatibleChat",
        "http://localhost:1234/v1",
        None,
    );
    assert!(is(lifecycle(&cx.connection("ollama")), "Connected"));
    // A model of LM Studio with a declared context window (schema revision 2).
    let model = json!({"id": "qwen-local", "displayName": "Qwen (LM Studio)",
        "endpointProfile": "CompatibleChat", "upstreamModelId": "qwen3.8-27b",
        "enabled": true, "tierMemberships": [], "efforts": [], "imageInput": false,
        "tools": true, "revision": "1", "contextTokens": "131072", "connectionId": "lmstudio"});
    assert!(summary(cx.configure(json!({"kind": "Model", "model": model}))).2);
    let catalog = match result(cx.query(
        &cx.user(),
        "router.catalog",
        json!({"page": {"limit": 100}, "includeDisabled": true}),
        Some(2),
    )) {
        OperationResult::RouterCatalog { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    let m = catalog
        .iter()
        .find(|m| m.id.as_str() == "qwen-local")
        .unwrap();
    assert_eq!(m.connection_id.as_ref().map(Id::as_str), Some("lmstudio"));
    assert_eq!(
        m.context_tokens.as_ref().map(|c| c.0.as_str()),
        Some("131072")
    );
    // Revision 1 readers never see the revision-2 members.
    let v1 = match result(cx.query(
        &cx.user(),
        "router.catalog",
        json!({"page": {"limit": 100}, "includeDisabled": true}),
        None,
    )) {
        OperationResult::RouterCatalog { value } => value.items.0,
        other => panic!("{other:?}"),
    };
    assert!(
        v1.iter()
            .all(|m| m.context_tokens.is_none() && m.connection_id.is_none())
    );
    let a = cx.availability("qwen-local");
    assert!(a.available);
    // Ollama is not where this model lives: LM Studio unreachable makes it unavailable even
    // though another compatible endpoint is up.
    cx.http.on(
        Method::Get,
        "http://localhost:11434/v1/models",
        200,
        json!({"data": [{"id": "llama3"}]}),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "lmstudio", "action": "Inspect"})),
    );
    let c = cx.connection("lmstudio");
    assert!(is(lifecycle(&c), "Unavailable"));
    assert_eq!(c.healthy, Some(false));
    let a = cx.availability("qwen-local");
    assert!(!a.available);
    assert!(codes(&a).contains(&"ConnectionUnavailable"));
    // Discovery recovers it.
    cx.http.on(
        Method::Get,
        "http://localhost:1234/v1/models",
        200,
        json!({"data": [{"id": "qwen3.8-27b"}]}),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "lmstudio", "action": "Inspect"})),
    );
    assert!(is(lifecycle(&cx.connection("lmstudio")), "Connected"));
    assert!(cx.availability("qwen-local").available);
    // A binding to a connection of another profile is refused.
    let wrong = json!({"id": "qwen-2", "displayName": "Qwen", "endpointProfile": "OpenRouterChat",
        "upstreamModelId": "q", "enabled": true, "tierMemberships": [], "efforts": [],
        "imageInput": false, "tools": true, "revision": "1", "connectionId": "lmstudio"});
    assert_eq!(
        refused(cx.configure(json!({"kind": "Model", "model": wrong}))).code,
        DomainErrorCode::InvalidArgument
    );
}

#[test]
fn tiers_and_decision_models_keep_their_own_homes() {
    let cx = Cx::new();
    // A catalog edit cannot change tier membership: the Tier action (routing settings) does.
    let opus = cx.read(|r| r.router().route_model(&id("opus"))).unwrap();
    let edit = json!({"id": "opus", "displayName": "Opus", "endpointProfile": "ClaudeSubscriptionMessages",
        "upstreamModelId": "claude-opus-5-5", "enabled": true, "tierMemberships": ["Max"],
        "efforts": ["Low", "Medium", "High", "XHigh", "Max"], "imageInput": true, "tools": true,
        "revision": "1"});
    assert_eq!(
        refused(cx.configure(json!({"kind": "Model", "model": edit,
        "expectedRevision": opus.revision.to_string()})))
        .code,
        DomainErrorCode::InvalidArgument
    );
    let settings = cx.read(|r| {
        r.configuration().current(
            &ConfigScope::Workspace,
            devworks_core::settings::model::NAMESPACE,
        )
    });
    let rev = settings.map_or(1, |s| s.revision);
    assert!(
        summary(cx.configure(json!({"kind": "Tier", "tier": "Max",
        "expectedRevision": rev.to_string(), "memberIds": ["opus", "astra"]})))
        .2
    );
    let members = cx.read(|r| devworks_core::services::router::tier_members(r.configuration()));
    assert_eq!(
        members["Max"].iter().map(Id::as_str).collect::<Vec<_>>(),
        ["opus", "astra"]
    );

    // SystemOne decision models: their own IDs on a SystemOne connection, never tier members.
    cx.add("systemone", "SystemOne", "http://localhost:8000/v1", None);
    let decision = |model_id: &str, connection: &str| {
        json!({"id": model_id, "revision": "1",
        "displayName": "Jev", "connectionId": connection, "upstreamModelId": "vllm-sr/jev",
        "questionTypes": ["choice", "score"], "acceptsImages": true, "acceptsVideos": false,
        "enabled": true, "extensionQuestionSchemas": [],
        "resultSchema": {"schemaId": "systemone/result", "revision": "1", "sha256": "a".repeat(64)}})
    };
    assert!(
        summary(
            cx.configure(json!({"kind": "DecisionModel", "model": decision("jev", "systemone")}))
        )
        .2
    );
    assert_eq!(
        refused(cx.configure(json!({"kind": "DecisionModel",
        "model": decision("opus", "systemone")})))
        .code,
        DomainErrorCode::Conflict
    );
    cx.add(
        "ollama",
        "CompatibleChat",
        "http://localhost:11434/v1",
        None,
    );
    assert_eq!(
        refused(cx.configure(json!({"kind": "DecisionModel",
        "model": decision("jev-2", "ollama")})))
        .code,
        DomainErrorCode::InvalidArgument
    );
    let rev = cx
        .read(|r| {
            r.configuration().current(
                &ConfigScope::Workspace,
                devworks_core::settings::model::NAMESPACE,
            )
        })
        .unwrap()
        .revision;
    assert_eq!(
        refused(cx.configure(json!({"kind": "Tier", "tier": "Low",
        "expectedRevision": rev.to_string(), "memberIds": ["jev"]})))
        .code,
        DomainErrorCode::NotFound
    );
    let stored = cx.read(|r| r.router().decision_model(&id("jev"))).unwrap();
    assert_eq!(stored.question_types, ["choice", "score"]);
}

#[test]
fn settings_secret_references_resolve_from_the_same_store_and_survive_reconciliation() {
    use devworks_core::ports::secrets::{SecretRef, SecretResolver, SecretUnavailable};
    let cx = Cx::new();
    let reference = cx
        .svc
        .stage_setting_secret(Secret::new("search-key-1"))
        .unwrap();
    assert!(reference.as_str().starts_with("dwsecret-"));
    assert_eq!(cx.svc.resolve(&reference).unwrap().expose(), "search-key-1");
    cx.advance(2 * 60 * 60 * 1000);
    cx.svc.reconcile().unwrap();
    assert_eq!(cx.svc.resolve(&reference).unwrap().expose(), "search-key-1");
    let missing = SecretRef::mint(7);
    assert_eq!(cx.svc.resolve(&missing), Err(SecretUnavailable::NotFound));
    cx.secrets.set_unavailable(Some("locked"));
    assert_eq!(cx.svc.resolve(&reference), Err(SecretUnavailable::Locked));
}

#[test]
fn unpublished_handles_are_reconciled_without_reading_values() {
    let cx = Cx::new();
    let kept = cx.stage("sk-kept");
    cx.add(
        "or",
        "OpenRouterChat",
        "https://openrouter.test/api/v1",
        Some(&kept),
    );
    let orphan = cx.stage("sk-orphan");
    // A fresh staged key survives a reconcile (its command may still come).
    assert_eq!(cx.svc.reconcile().unwrap(), 0);
    cx.advance(31 * 60 * 1000);
    assert_eq!(cx.svc.reconcile().unwrap(), 1);
    assert_eq!(cx.secret(&orphan), None);
    assert!(cx.secret(&kept).is_some());
    // A handle written by a process that crashed before publishing (no staging record).
    let ns = cx.store.info().store_id.0.as_str().to_owned();
    cx.secrets
        .put(
            &SecretKey::new(ns, id("credential-lost")),
            &Secret::new("x"),
        )
        .unwrap();
    let cx = cx.restart();
    assert_eq!(cx.svc.reconcile().unwrap(), 1);
    assert!(cx.secret(&kept).is_some());
}

#[test]
fn an_unavailable_secret_store_keeps_credentials_unavailable_not_lost() {
    let cx = Cx::new();
    let key = cx.stage("sk-x");
    cx.add(
        "or",
        "OpenRouterChat",
        "https://openrouter.test/api/v1",
        Some(&key),
    );
    assert_eq!(cx.svc.secret_store_unavailable(), None);
    cx.secrets.set_unavailable(Some("locked"));
    // A credential read (a routed request, a test, a refresh) records the facility's state,
    // structured, for Settings to offer the session-only fallback.
    assert!(matches!(
        cx.svc.access(&id("or"), None),
        Err(AccessError::Unavailable(_))
    ));
    assert_eq!(cx.svc.secret_store_unavailable().as_deref(), Some("locked"));
    cx.secrets.set_unavailable(None);
    assert!(cx.svc.access(&id("or"), None).is_ok());
    assert_eq!(
        cx.svc.secret_store_unavailable(),
        None,
        "a success clears it"
    );
    // Staging a key records it too.
    cx.secrets.set_unavailable(Some("locked"));
    assert!(
        cx.svc
            .stage_api_key(&actor("user-a"), Secret::new("sk-y"))
            .is_err()
    );
    assert_eq!(cx.svc.secret_store_unavailable().as_deref(), Some("locked"));
    assert!(matches!(
        cx.svc.access(&id("or"), None),
        Err(AccessError::Unavailable(_))
    ));
    // Nothing durable changed: the store being locked is not a revocation.
    assert!(is(lifecycle(&cx.connection("or")), "Connected"));
    cx.secrets.set_unavailable(None);
    assert!(matches!(
        cx.svc.access(&id("or"), None).unwrap().credential,
        AccessCredential::ApiKey(_)
    ));
}

#[test]
fn concurrent_requests_share_one_token_refresh() {
    let cx = Arc::new(Cx::new());
    cx.add("claude", "ClaudeSubscriptionMessages", ANTHROPIC, None);
    let c = cx.connection("claude");
    summary(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true})),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude", "action": "BeginSignIn"})),
    );
    let flow = cx.connection("claude").sign_in.unwrap();
    let state = flow
        .authorize_url
        .unwrap()
        .0
        .split("state=")
        .nth(1)
        .unwrap()
        .to_owned();
    cx.http.on(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-1",
        "refresh_token": "rt-1", "expires_in": 60}),
    );
    cx.svc
        .submit_authorization_code(
            &actor("user-a"),
            &flow.flow_id,
            Secret::new(format!("c#{state}")),
        )
        .unwrap();
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "CompleteSignIn", "flowId": flow.flow_id.as_str()})),
    );
    // Inside the refresh skew: every caller needs a refresh, one performs it.
    cx.http.on(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-2",
        "refresh_token": "rt-2", "expires_in": 3600}),
    );
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let cx = cx.clone();
            std::thread::spawn(move || cx.svc.access(&id("claude"), None).unwrap())
        })
        .collect();
    for t in threads {
        let a = t.join().unwrap();
        assert!(
            matches!(a.credential, AccessCredential::Bearer { token, .. } if token.expose() == "at-2")
        );
    }
    let refreshes = cx
        .http
        .requests(CLAUDE_TOKEN)
        .iter()
        .filter(|r| body_json(r)["grant_type"] == "refresh_token")
        .count();
    assert_eq!(refreshes, 1);
    let _ = T0;
    let _ = cx.now();
}

#[test]
fn migration_17_is_additive_and_corrects_only_unedited_seed_efforts() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 17)
        .expect("migration 17");
    assert_eq!(
        m.checksum(),
        MIGRATION_0017_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 14));
    let cx = Cx::new();
    let opus = cx.read(|r| r.router().route_model(&id("opus"))).unwrap();
    assert_eq!(opus.efforts, ["Low", "Medium", "High", "XHigh", "Max"]);
    let sol = cx.read(|r| r.router().route_model(&id("sol"))).unwrap();
    assert_eq!(sol.efforts, ["Low", "Medium", "High", "XHigh"]);
    assert_eq!(opus.connection_id, None);
    let _: &SqliteStore = &cx.store;
}

/// A catalog model of the OpenRouter profile.
fn open_router_model(cx: &Cx, model: &str, upstream: &str) {
    let m = json!({"id": model, "displayName": model, "endpointProfile": "OpenRouterChat",
        "upstreamModelId": upstream, "enabled": true, "tierMemberships": [],
        "efforts": [], "imageInput": false, "tools": true, "revision": "1"});
    assert!(summary(cx.configure(json!({"kind": "Model", "model": m}))).2);
}
fn edit_connection(
    cx: &Cx,
    connection: &str,
    profile: &str,
    endpoint: &str,
    extra: Value,
) -> DomainCommandReply {
    let c = cx.connection(connection);
    let mut action = json!({"kind": "Connection", "connectionId": connection,
        "expectedRevision": c.revision.get().to_string(), "name": c.name.0,
        "profile": profile, "endpoint": endpoint});
    for (k, v) in extra.as_object().unwrap() {
        action[k] = v.clone();
    }
    cx.configure(action)
}

#[test]
fn open_router_constraints_filter_discovery_and_routing_and_travel_with_each_request() {
    let cx = Cx::new();
    let key = cx.stage("sk-or");
    cx.add("or", "OpenRouterChat", OPENROUTER, Some(&key));
    let c = cx.connection("or");
    assert_eq!(c.enabled, Some(true));
    assert!(c.open_router.is_none());
    open_router_model(&cx, "or-opus", "anthropic/claude-opus-5-5");
    open_router_model(&cx, "or-haiku", "anthropic/claude-haiku-5-5");
    open_router_model(&cx, "or-llama", "meta/llama-5");
    assert!(cx.availability("or-haiku").available);

    let constraints = json!({
        "allowedModels": ["anthropic/*", "openrouter/*"],
        "excludedModels": ["anthropic/claude-haiku*"],
        "costTier": "low",
        "provider": {"order": ["anthropic", "google-vertex"], "allowFallbacks": false,
            "requireParameters": true, "dataCollection": "deny", "zdr": true,
            "enforceDistillableText": false, "only": ["anthropic"], "ignore": ["deepinfra"],
            "quantizations": ["fp8", "bf16"], "sort": {"by": "price", "partition": "model"},
            "preferredMinThroughput": {"p90": 50.0}, "preferredMaxLatency": {"p50": 1.5},
            "maxPrice": {"prompt": "1.5", "completion": "10"}}});
    assert!(
        summary(edit_connection(
            &cx,
            "or",
            "OpenRouterChat",
            OPENROUTER,
            json!({"openRouter": constraints})
        ))
        .2
    );
    let c = cx.connection("or");
    let stored = c
        .open_router
        .as_ref()
        .expect("revision 2 reports the constraints");
    assert_eq!(serde_json::to_value(stored).unwrap(), constraints);
    // An equal resend changes nothing.
    assert!(
        !summary(edit_connection(
            &cx,
            "or",
            "OpenRouterChat",
            OPENROUTER,
            json!({"openRouter": constraints})
        ))
        .2
    );

    // Routing: an excluded or not-allowed model is ineligible, and says why.
    assert!(cx.availability("or-opus").available);
    for blocked in ["or-haiku", "or-llama"] {
        let a = cx.availability(blocked);
        assert!(!a.available);
        assert!(codes(&a).contains(&"NotAllowed"), "{blocked}: {a:?}");
    }

    // Discovery: excluded models are not listed as offered.
    cx.http.on(
        Method::Get,
        &format!("{OPENROUTER}/models"),
        200,
        json!({"data": [
        {"id": "anthropic/claude-opus-5-5"}, {"id": "anthropic/claude-haiku-5-5"},
        {"id": "meta/llama-5"}, {"id": "openrouter/auto"}]}),
    );
    summary(cx.configure(json!({"kind": "SignIn", "connectionId": "or", "action": "Inspect"})));
    let c = cx.connection("or");
    let ids: Vec<&str> = c
        .discovered_model_ids
        .as_ref()
        .unwrap()
        .0
        .iter()
        .map(|t| t.0.as_str())
        .collect();
    assert_eq!(ids, ["anthropic/claude-opus-5-5", "openrouter/auto"]);

    // The transport receives them with every request's credential (one read), as OpenRouter's
    // own wire members.
    let a = cx.svc.access(&id("or"), None).unwrap();
    let o = a
        .open_router
        .expect("the constraints travel with the credential");
    assert_eq!(
        Value::Object(open_router_request_fields(&o, "anthropic/claude-opus-5-5")),
        json!({
        "provider": {"order": ["anthropic", "google-vertex"], "allow_fallbacks": false,
            "require_parameters": true, "data_collection": "deny", "zdr": true,
            "enforce_distillable_text": false, "only": ["anthropic"], "ignore": ["deepinfra"],
            "quantizations": ["fp8", "bf16"], "sort": {"by": "price", "partition": "model"},
            "preferred_min_throughput": {"p90": 50.0}, "preferred_max_latency": {"p50": 1.5},
            "max_price": {"prompt": 1.5, "completion": 10.0}}})
    );
    let auto = open_router_request_fields(&o, "openrouter/auto");
    assert_eq!(
        auto["plugins"],
        json!([{"id": "auto-router", "cost_tier": "low",
        "allowed_models": ["anthropic/*", "openrouter/*"],
        "excluded_models": ["anthropic/claude-haiku*"]}])
    );

    // Only an OpenRouter connection takes them; patterns are checked.
    let anthropic = cx.stage("sk-ant");
    cx.add(
        "anthropic",
        "AnthropicMessages",
        ANTHROPIC,
        Some(&anthropic),
    );
    let f = refused(edit_connection(
        &cx,
        "anthropic",
        "AnthropicMessages",
        ANTHROPIC,
        json!({"openRouter": {"allowedModels": [], "excludedModels": []}}),
    ));
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);
    let f = refused(edit_connection(
        &cx,
        "or",
        "OpenRouterChat",
        OPENROUTER,
        json!({"openRouter": {"allowedModels": ["anthropic /x"], "excludedModels": []}}),
    ));
    assert_eq!(f.code, DomainErrorCode::InvalidArgument);

    // An empty set clears them. The filtered list is dropped with them (it was made under the
    // old constraints), so the excluded model is eligible again until the next discovery.
    assert!(
        summary(edit_connection(
            &cx,
            "or",
            "OpenRouterChat",
            OPENROUTER,
            json!({"openRouter": {"allowedModels": [], "excludedModels": []}})
        ))
        .2
    );
    let c = cx.connection("or");
    assert!(c.open_router.is_none());
    assert!(c.discovered_model_ids.is_none());
    let a = cx.availability("or-haiku");
    assert!(a.available, "{a:?}");
}

#[test]
fn a_turned_off_connection_keeps_its_credential_and_is_never_routed_through() {
    let cx = Cx::new();
    let key = cx.stage("sk-ant-off");
    cx.add("anthropic", "AnthropicMessages", ANTHROPIC, Some(&key));
    let model = json!({"id": "opus-api", "displayName": "Opus (API)",
        "endpointProfile": "AnthropicMessages", "upstreamModelId": "claude-opus-5-5",
        "enabled": true, "tierMemberships": [], "efforts": ["Low", "High"],
        "imageInput": true, "tools": true, "revision": "1"});
    assert!(summary(cx.configure(json!({"kind": "Model", "model": model}))).2);
    assert!(cx.availability("opus-api").available);

    assert!(
        summary(edit_connection(
            &cx,
            "anthropic",
            "AnthropicMessages",
            ANTHROPIC,
            json!({"enabled": false})
        ))
        .2
    );
    let c = cx.connection("anthropic");
    assert_eq!(c.enabled, Some(false));
    assert!(is(lifecycle(&c), "Disabled"));
    assert_eq!(
        c.state_reason.as_ref().map(|r| r.0.as_str()),
        Some("the connection is turned off")
    );
    // The credential is kept.
    assert_eq!(c.credential_ref.as_ref(), Some(&key));
    assert!(cx.secret(&key).is_some());
    // Never routed through, and the router says so.
    let a = cx.availability("opus-api");
    assert!(!a.available);
    assert!(codes(&a).contains(&"ConnectionDisabled"), "{a:?}");
    assert_eq!(
        cx.svc.access(&id("anthropic"), None).err(),
        Some(AccessError::Disabled)
    );
    // An edit that does not name it keeps it off.
    summary(edit_connection(
        &cx,
        "anthropic",
        "AnthropicMessages",
        ANTHROPIC,
        json!({"name": "Anthropic API"}),
    ));
    assert_eq!(cx.connection("anthropic").enabled, Some(false));
    // Turned on again: routable with the same credential.
    assert!(
        summary(edit_connection(
            &cx,
            "anthropic",
            "AnthropicMessages",
            ANTHROPIC,
            json!({"enabled": true})
        ))
        .2
    );
    let c = cx.connection("anthropic");
    assert_eq!(c.enabled, Some(true));
    assert_eq!(c.credential_ref.as_ref(), Some(&key));
    assert!(cx.availability("opus-api").available);
    assert!(cx.svc.access(&id("anthropic"), None).is_ok());
}

#[test]
fn a_staged_subscription_token_is_never_published_as_an_api_key() {
    let cx = Cx::new();
    // Exactly the handle a sign-in exchange stages for its tokens.
    let handle = cx
        .svc
        .stage_tokens_for_test(
            &actor("user-a"),
            Tokens {
                access: Secret::new("at-internal"),
                refresh: Secret::new("rt-internal"),
                expires_at_ms: cx.now() + 3_600_000,
                account: None,
            },
        )
        .unwrap();
    // Staged by the same principal, offered to an API-key profile: still not a key.
    let f = refused(
        cx.configure(json!({"kind": "Connection", "connectionId": "or",
        "name": "OpenRouter", "profile": "OpenRouterChat", "endpoint": OPENROUTER,
        "credentialRef": handle.as_str()})),
    );
    assert_eq!(f.code, DomainErrorCode::NotFound);
    assert!(cx.read(|r| r.router().connection(&id("or"))).is_none());
}

#[test]
fn a_periodic_check_sends_each_credential_only_to_the_endpoint_read_with_it() {
    let cx = Cx::new();
    let a_key = cx.stage("sk-a");
    let b_old = cx.stage("sk-b-old");
    cx.add(
        "a-or",
        "OpenRouterChat",
        "https://a.openrouter.test/api/v1",
        Some(&a_key),
    );
    cx.add(
        "b-or",
        "OpenRouterChat",
        "https://b.openrouter.test/api/v1",
        Some(&b_old),
    );
    for host in ["a", "b", "c"] {
        cx.http.on(
            Method::Get,
            &format!("https://{host}.openrouter.test/api/v1/models"),
            200,
            json!({"data": [{"id": format!("x/{host}")}]}),
        );
    }
    cx.advance(DISCOVERY_INTERVAL_MS + 1);
    let b_new = cx.stage("sk-b-new");
    let a_new = cx.stage("sk-a-new");
    // The tick reads both rows, then checks them one by one. While a-or's request is in flight
    // a-or gets a new key and b-or moves to another origin with a new key (concurrent
    // configures).
    let store = cx.store.clone();
    let moved = b_new.clone();
    cx.http.during("https://a.openrouter.test", move || {
        run_write(&*store, WriteOptions::default(), move |tx| {
            let mut a = tx.router().connection(&id("a-or"))?.unwrap();
            let expected = a.revision;
            a.credential_ref = Some(a_new);
            a.credential_revision += 1;
            tx.router().put_connection(&a, Some(expected))?;
            let mut c = tx.router().connection(&id("b-or"))?.unwrap();
            let expected = c.revision;
            c.endpoint = "https://c.openrouter.test/api/v1".into();
            c.credential_ref = Some(moved);
            c.credential_revision += 1;
            tx.router().put_connection(&c, Some(expected))?;
            Ok(())
        })
        .unwrap();
    });
    cx.svc.tick();
    // b-or's check was built from one read: the new key went to the new origin only.
    assert!(
        cx.http.requests("https://b.openrouter.test").is_empty(),
        "nothing is sent to the endpoint the stale row named"
    );
    let sent = cx.http.requests("https://c.openrouter.test");
    assert_eq!(sent.len(), 1);
    assert_eq!(header(&sent[0], "authorization"), Some("Bearer sk-b-new"));
    // And the result is recorded against the credential it was made with.
    let c = cx.connection("b-or");
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(2));
    assert_eq!(c.healthy, Some(true));
    let ids: Vec<&str> = c
        .discovered_model_ids
        .as_ref()
        .unwrap()
        .0
        .iter()
        .map(|t| t.0.as_str())
        .collect();
    assert_eq!(ids, ["x/c"]);
    // a-or's answer was for the key it replaced: not recorded for the new one.
    let a = cx.connection("a-or");
    assert_eq!(a.credential_revision.as_ref().map(Counter::get), Some(2));
    assert!(a.healthy.is_none() && a.checked_at_ms.is_none(), "{a:?}");
}

#[test]
fn a_refresh_records_the_models_listed_with_the_refreshed_token() {
    let cx = Cx::new();
    cx.add("codex", "CodexSubscriptionResponses", CODEX_API, None);
    cx.http.on(
        Method::Post,
        &format!("{CODEX_ISSUER}/api/accounts/deviceauth/usercode"),
        200,
        json!({"device_auth_id": "dev-1", "user_code": "ABCD-EFGH", "interval": "5"}),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "codex", "action": "BeginSignIn"})),
    );
    let poll = format!("{CODEX_ISSUER}/api/accounts/deviceauth/token");
    cx.http.on(
        Method::Post,
        &poll,
        200,
        json!({"authorization_code": "auth-1", "code_verifier": "ver-1"}),
    );
    let id_token = jwt(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-9"}}));
    let token = format!("{CODEX_ISSUER}/oauth/token");
    cx.http.on(
        Method::Post,
        &token,
        200,
        json!({"access_token": "codex-at",
        "refresh_token": "codex-rt", "id_token": id_token, "expires_in": 3600}),
    );
    cx.http.on(
        Method::Get,
        &format!("{CODEX_API}/models"),
        200,
        json!({"models": [{"slug": "gpt-6-luna"}]}),
    );
    cx.advance(5_000);
    cx.svc.tick();
    let c = cx.connection("codex");
    assert!(is(lifecycle(&c), "Connected"), "{c:?}");
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(1));
    assert_eq!(c.discovered_model_ids.as_ref().map(|i| i.0.len()), Some(1));

    // Refresh: a forced token refresh (a new credential revision), then the list with it.
    cx.http.on(
        Method::Post,
        &token,
        200,
        json!({"access_token": "codex-at-2",
        "refresh_token": "codex-rt-2", "id_token": id_token, "expires_in": 3600}),
    );
    cx.http.reset(
        Method::Get,
        &format!("{CODEX_API}/models"),
        200,
        json!({"models": [{"slug": "gpt-6-luna"}, {"slug": "gpt-6.1-sol"}]}),
    );
    assert!(
        summary(
            cx.configure(json!({"kind": "SignIn", "connectionId": "codex",
        "action": "Refresh"}))
        )
        .2
    );
    let lists = cx.http.requests(&format!("{CODEX_API}/models"));
    assert_eq!(
        header(lists.last().unwrap(), "authorization"),
        Some("Bearer codex-at-2")
    );
    let c = cx.connection("codex");
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(2));
    assert_eq!(
        c.discovered_model_ids.as_ref().map(|i| i.0.len()),
        Some(2),
        "the list made with the refreshed token is recorded for it"
    );
}

#[test]
fn a_replaced_key_forgets_the_model_list_made_with_the_old_one() {
    let cx = Cx::new();
    let first = cx.stage("sk-ant-first");
    cx.add("anthropic", "AnthropicMessages", ANTHROPIC, Some(&first));
    for (m, up) in [
        ("opus-api", "claude-opus-5-5"),
        ("haiku-api", "claude-haiku-5-5"),
    ] {
        let model = json!({"id": m, "displayName": m, "endpointProfile": "AnthropicMessages",
            "upstreamModelId": up, "enabled": true, "tierMemberships": [], "efforts": [],
            "imageInput": false, "tools": true, "revision": "1"});
        assert!(summary(cx.configure(json!({"kind": "Model", "model": model}))).2);
    }
    // The first key's account lists only Opus.
    cx.http.on(
        Method::Get,
        &format!("{ANTHROPIC}/v1/models"),
        200,
        json!({"data": [{"id": "claude-opus-5-5"}], "has_more": false}),
    );
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "anthropic", "action": "Inspect"})),
    );
    assert!(codes(&cx.availability("haiku-api")).contains(&"NotDiscovered"));

    // Another key (perhaps another account): nothing learned with the first one is kept.
    let second = cx.stage("sk-ant-second");
    assert!(
        summary(edit_connection(
            &cx,
            "anthropic",
            "AnthropicMessages",
            ANTHROPIC,
            json!({"credentialRef": second.as_str()})
        ))
        .2
    );
    let c = cx.connection("anthropic");
    assert_eq!(c.credential_revision.as_ref().map(Counter::get), Some(2));
    assert!(c.discovered_model_count.is_none() && c.discovered_at_ms.is_none());
    assert!(c.healthy.is_none() && c.checked_at_ms.is_none());
    let a = cx.availability("haiku-api");
    assert!(a.available, "{a:?}");

    // Removing the key forgets the list made with it too.
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "anthropic", "action": "Inspect"})),
    );
    assert!(cx.connection("anthropic").discovered_model_count.is_some());
    summary(
        cx.configure(
            json!({"kind": "SignIn", "connectionId": "anthropic", "action": "Disconnect"}),
        ),
    );
    let c = cx.connection("anthropic");
    assert!(c.discovered_model_count.is_none() && c.discovered_at_ms.is_none());
}

#[test]
fn a_keyless_endpoint_that_refuses_authorization_is_not_routed_through() {
    let cx = Cx::new();
    let endpoint = "https://models.example.test/v1";
    cx.add("vllm", "CompatibleChat", endpoint, None);
    let model = json!({"id": "qwen-remote", "displayName": "Qwen", "endpointProfile":
        "CompatibleChat", "upstreamModelId": "qwen3.8-27b", "enabled": true,
        "tierMemberships": [], "efforts": [], "imageInput": false, "tools": true,
        "revision": "1", "connectionId": "vllm"});
    assert!(summary(cx.configure(json!({"kind": "Model", "model": model}))).2);
    assert!(cx.availability("qwen-remote").available);

    // The endpoint starts demanding a key: unusable for routing, and it says why.
    cx.http
        .on(Method::Get, &format!("{endpoint}/models"), 401, json!({}));
    summary(cx.configure(json!({"kind": "SignIn", "connectionId": "vllm", "action": "Inspect"})));
    let c = cx.connection("vllm");
    assert!(is(lifecycle(&c), "ReauthenticationRequired"), "{c:?}");
    assert_eq!(c.healthy, Some(false));
    assert!(
        c.state_reason
            .as_ref()
            .is_some_and(|r| r.0.contains("add its API key"))
    );
    assert!(!cx.availability("qwen-remote").available);
    assert!(matches!(
        cx.svc.access(&id("vllm"), None),
        Err(AccessError::ReauthenticationRequired(_))
    ));

    // It accepts requests again: a test brings it back.
    cx.http.reset(
        Method::Get,
        &format!("{endpoint}/models"),
        200,
        json!({"data": [{"id": "qwen3.8-27b"}]}),
    );
    summary(cx.configure(json!({"kind": "SignIn", "connectionId": "vllm", "action": "Inspect"})));
    assert!(is(lifecycle(&cx.connection("vllm")), "Connected"));
    assert!(cx.availability("qwen-remote").available);
    assert!(cx.svc.access(&id("vllm"), None).is_ok());
}

#[test]
fn a_credential_replaced_while_it_is_read_is_read_again_not_reported_lost() {
    let cx = Cx::new();
    let first = cx.stage("sk-first");
    cx.add("or", "OpenRouterChat", OPENROUTER, Some(&first));
    let second = cx.stage("sk-second");
    let ns = cx.store.info().store_id.0.as_str().to_owned();
    // While the first key is read, a replacement publishes the second and retires the first.
    let (store, secrets, ns2) = (cx.store.clone(), cx.secrets.clone(), ns.clone());
    let (old, new) = (first.clone(), second.clone());
    cx.hooked.during_get(&first, move || {
        run_write(&*store, WriteOptions::default(), move |tx| {
            let mut c = tx.router().connection(&id("or"))?.unwrap();
            let expected = c.revision;
            c.credential_ref = Some(new);
            c.credential_revision += 1;
            tx.router().put_connection(&c, Some(expected))?;
            Ok(())
        })
        .unwrap();
        secrets.delete(&SecretKey::new(ns2, old)).unwrap();
    });
    let a = cx
        .svc
        .access(&id("or"), None)
        .expect("the replacement is used");
    assert!(matches!(&a.credential, AccessCredential::ApiKey(k) if k.expose() == "sk-second"));
    assert_eq!(a.credential_revision, 2);
    assert!(is(lifecycle(&cx.connection("or")), "Connected"));
    // A missing credential that is still the connection's own is reported as lost.
    cx.secrets.delete(&SecretKey::new(ns, second)).unwrap();
    assert!(matches!(
        cx.svc.access(&id("or"), None),
        Err(AccessError::ReauthenticationRequired(_))
    ));
    assert!(is(
        lifecycle(&cx.connection("or")),
        "ReauthenticationRequired"
    ));
}

/// Begins and completes a Claude subscription sign-in whose token expires in `expires_in` s.
fn claude_sign_in(cx: &Cx, access: &str, expires_in: i64) {
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude", "action": "BeginSignIn"})),
    );
    let flow = cx.connection("claude").sign_in.unwrap();
    let state = flow
        .authorize_url
        .unwrap()
        .0
        .split("state=")
        .nth(1)
        .unwrap()
        .to_owned();
    cx.http.reset(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": access,
        "refresh_token": format!("rt-{access}"), "expires_in": expires_in}),
    );
    cx.svc
        .submit_authorization_code(
            &actor("user-a"),
            &flow.flow_id,
            Secret::new(format!("c#{state}")),
        )
        .unwrap();
    summary(
        cx.configure(json!({"kind": "SignIn", "connectionId": "claude",
        "action": "CompleteSignIn", "flowId": flow.flow_id.as_str()})),
    );
}

#[test]
fn unknown_refresh_outcomes_are_counted_per_credential() {
    let cx = Cx::new();
    cx.add("claude", "ClaudeSubscriptionMessages", ANTHROPIC, None);
    let c = cx.connection("claude");
    summary(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true})),
    );
    // Inside the refresh skew, so every use refreshes; the refresh's outcome is unknown.
    claude_sign_in(&cx, "at-1", 60);
    cx.http.reset(Method::Post, CLAUDE_TOKEN, 0, json!({}));
    assert!(matches!(
        cx.svc.access(&id("claude"), None),
        Err(AccessError::Unavailable(_))
    ));
    // A new sign-in is another credential: its first unknown outcome is its first.
    claude_sign_in(&cx, "at-2", 60);
    cx.http.reset(Method::Post, CLAUDE_TOKEN, 0, json!({}));
    assert!(matches!(
        cx.svc.access(&id("claude"), None),
        Err(AccessError::Unavailable(_))
    ));
    assert!(is(lifecycle(&cx.connection("claude")), "Connected"));
    // Its second requires a new sign-in.
    assert!(matches!(
        cx.svc.access(&id("claude"), None),
        Err(AccessError::ReauthenticationRequired(_))
    ));
}

#[test]
fn a_refresh_overtaken_by_a_new_sign_in_uses_the_new_credential() {
    let cx = Cx::new();
    cx.add("claude", "ClaudeSubscriptionMessages", ANTHROPIC, None);
    let c = cx.connection("claude");
    summary(
        cx.configure(json!({"kind": "SubscriptionRisk", "connectionId": "claude",
        "expectedRevision": c.revision.get().to_string(), "noticeSha256": risk_notice_sha256(),
        "accepted": true})),
    );
    claude_sign_in(&cx, "at-old", 60);
    // A sign-in completes while the old token's refresh is in flight.
    let fresh = cx
        .svc
        .stage_tokens_for_test(
            &actor("user-a"),
            Tokens {
                access: Secret::new("at-new"),
                refresh: Secret::new("rt-new"),
                expires_at_ms: cx.now() + 3_600_000,
                account: None,
            },
        )
        .unwrap();
    let store = cx.store.clone();
    cx.http.reset(
        Method::Post,
        CLAUDE_TOKEN,
        200,
        json!({"access_token": "at-rotated",
        "refresh_token": "rt-rotated", "expires_in": 3600}),
    );
    cx.http.during(CLAUDE_TOKEN, move || {
        run_write(&*store, WriteOptions::default(), move |tx| {
            let mut c = tx.router().connection(&id("claude"))?.unwrap();
            let expected = c.revision;
            c.credential_ref = Some(fresh);
            c.credential_revision += 1;
            tx.router().put_connection(&c, Some(expected))?;
            Ok(())
        })
        .unwrap();
    });
    let a = cx
        .svc
        .access(&id("claude"), None)
        .expect("the new sign-in's token is used");
    assert!(
        matches!(&a.credential, AccessCredential::Bearer { token, .. } if token.expose() == "at-new")
    );
}
