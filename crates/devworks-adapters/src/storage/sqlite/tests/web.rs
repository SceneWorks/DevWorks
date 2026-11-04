//! The shared web tools end to end (DW-51): `web.fetch` and `web.search` through the gateway and
//! host authority on the real SQLite store, real managed artifact files and the real native
//! HTTP transport, against in-process local servers. Public addresses are scripted by a resolver
//! and mapped onto the local server by a recording transport wrapper, so the egress policy sees
//! exactly what it would see on the internet while no request leaves the machine. No real
//! network, no inference.
use super::Harness;
use crate::fs::NativeFilesystem;
use crate::fs::artifacts::NativeArtifactFiles;
use crate::fs::sources::HostArtifactSources;
use crate::storage::sqlite::SqliteStore;
use crate::web::brave::BraveSearch;
use crate::web::transport::NativeTransport;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::redact::SecretRef;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::ArtifactFiles;
use devworks_core::ports::secrets::{SecretResolver, SecretUnavailable, SecretValue};
use devworks_core::ports::storage::*;
use devworks_core::ports::web::{
    HopRequest, HopResponse, HttpTransport, NetFault, Resolver, SearchProvider,
};
use devworks_core::protocol::records::{SettingAssignment, TrustLevel, WebDocument, WebResult};
use devworks_core::protocol::results::{DomainQueryReply, OperationResult};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, Counter, DomainErrorCode, DomainFault, Id, ProjectId, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::diagnostics::DiagnosticService;
use devworks_core::services::web::{WebPorts, WebService};
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::model;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}
fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// A documentation-range-free public address the scripted resolver hands out.
const PUBLIC: &str = "93.184.216.34";
const PUBLIC_2: &str = "93.184.216.35";
const SEARCH_ADDR: &str = "151.101.1.1";
const KEY: &str = "brave-test-key-0123456789";

// ---------------------------------------------------------------------------------------------
// The local server

/// What the server does with one request.
enum Reply {
    Bytes(Vec<u8>),
    /// Reads the request and never answers.
    Hang,
}

/// A recorded request head.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    head: String,
}

type Routes = Arc<dyn Fn(&str) -> Reply + Send + Sync>;

struct Server {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}
impl Server {
    fn start(routes: Routes) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (routes, log) = (routes.clone(), log.clone());
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let path = head.split(' ').nth(1).unwrap_or("").to_owned();
                    log.lock().unwrap().push(Seen {
                        path: path.clone(),
                        head,
                    });
                    match routes(&path) {
                        Reply::Bytes(b) => {
                            let _ = stream.write_all(&b);
                            let _ = stream.flush();
                        }
                        Reply::Hang => std::thread::sleep(Duration::from_secs(8)),
                    }
                });
            }
        });
        Self { addr, seen }
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    Reply::Bytes(bytes)
}
fn redirect(status: &str, location: &str) -> Reply {
    response(status, &[("Location", location)], b"")
}

// ---------------------------------------------------------------------------------------------
// Scripted ports

/// Answers names from a script; an answer list is consumed one per call (the last repeats), so
/// a rebinding resolver can be scripted.
#[derive(Default)]
struct Names {
    answers: Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
    calls: Mutex<Vec<String>>,
}
impl Names {
    fn set(&self, name: &str, answers: &[&[&str]]) {
        self.answers.lock().unwrap().insert(
            name.to_owned(),
            answers
                .iter()
                .map(|a| a.iter().map(|s| ip(s)).collect())
                .collect(),
        );
    }
    fn calls(&self, name: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == name)
            .count()
    }
}
impl Resolver for Names {
    fn resolve(
        &self,
        host: &str,
        _: u16,
        _: Instant,
        _: &[devworks_core::ports::storage::Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        self.calls.lock().unwrap().push(host.to_owned());
        let mut all = self.answers.lock().unwrap();
        match all.get_mut(host) {
            Some(list) if list.len() > 1 => Ok(list.remove(0)),
            Some(list) => Ok(list[0].clone()),
            None => Err(NetFault::NotSent {
                reason: format!("{host}: unknown name"),
                timed_out: false,
            }),
        }
    }
}

/// The real transport, with scripted public addresses mapped onto local servers. It records
/// every address it was asked to connect to (the checked one) and the headers of the request.
struct Mapped {
    inner: NativeTransport,
    map: Mutex<HashMap<IpAddr, SocketAddr>>,
    asked: Mutex<Vec<SocketAddr>>,
    headers: Mutex<Vec<Vec<(String, String)>>>,
}
impl Mapped {
    fn new() -> Self {
        Self {
            inner: NativeTransport::new(),
            map: Mutex::new(HashMap::new()),
            asked: Mutex::new(Vec::new()),
            headers: Mutex::new(Vec::new()),
        }
    }
    fn map(&self, public: &str, to: SocketAddr) {
        self.map.lock().unwrap().insert(ip(public), to);
    }
}
impl HttpTransport for Mapped {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        self.asked.lock().unwrap().push(request.addr);
        self.headers.lock().unwrap().push(
            request
                .headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        let to = self.map.lock().unwrap().get(&request.addr.ip()).copied();
        let mut r = request.clone();
        if let Some(to) = to {
            r.addr = to;
        }
        self.inner.get(&r)
    }
}

#[derive(Default)]
struct Secrets(Mutex<HashMap<String, String>>);
impl SecretResolver for Secrets {
    fn resolve(&self, r: &SecretRef) -> Result<SecretValue, SecretUnavailable> {
        self.0
            .lock()
            .unwrap()
            .get(r.as_str())
            .map(|v| SecretValue::new(v.clone()))
            .ok_or(SecretUnavailable::NotFound)
    }
}

// ---------------------------------------------------------------------------------------------
// The fixture

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    files: Arc<NativeArtifactFiles>,
    web: Arc<WebService>,
    cases: UseCases,
    names: Arc<Names>,
    net: Arc<Mapped>,
    secrets: Arc<Secrets>,
    n: Arc<AtomicU64>,
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let store = Arc::new(h.store());
        let files = Arc::new(NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.with_extension("cache"),
        ));
        let artifacts = Arc::new(ArtifactService::new(
            files.clone(),
            Arc::new(HostArtifactSources::new(h.clock.clone(), h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
            h.clock.clone(),
            h.ids.clone(),
        ));
        let diagnostics = Arc::new(DiagnosticService::new(
            store.clone(),
            h.ids.clone(),
            h.clock.clone(),
            None,
        ));
        let names = Arc::new(Names::default());
        let net = Arc::new(Mapped::new());
        let secrets = Arc::new(Secrets::default());
        let providers: Vec<Arc<dyn SearchProvider>> = vec![Arc::new(
            BraveSearch::with_endpoint(
                names.clone(),
                net.clone(),
                "http://search.test/res/v1/web/search",
            )
            .unwrap(),
        )];
        let web = Arc::new(WebService::new(
            store.clone(),
            artifacts,
            diagnostics,
            h.clock.clone(),
            h.ids.clone(),
            WebPorts {
                resolver: names.clone(),
                transport: net.clone(),
                providers,
                secrets: secrets.clone(),
            },
        ));
        let cases = UseCases::new().with_web(web.clone());
        let fx = Self {
            h,
            store,
            files,
            web,
            cases,
            names,
            net,
            secrets,
            n: Arc::new(AtomicU64::new(0)),
        };
        fx.store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);
                     INSERT INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Feature A',1,0,1,1,1,'{}');",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
                ("reviewer-a", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        for (who, role) in [
            ("agent-a", AgentRole::Worker),
            ("reviewer-a", AgentRole::CodeReviewer),
        ] {
            let by = fx.user();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind: PrincipalKind::Agent,
                role: Some(role),
                project_id: Some(project("project-a")),
                tool_families: None,
                bound_by: actor("user-a"),
            };
            fx.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        fx
    }

    fn ctx(&self, who: &str, kind: PrincipalKind, p: Option<&str>) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store)
            .context(
                actor(who),
                kind,
                id("invocation-1"),
                id("ui-main"),
                Counter::new(info.restore_epoch).unwrap(),
                Revision::new(1).unwrap(),
            )
            .with_lineage(p.map(project), None, None)
    }
    fn user(&self) -> InvocationContext {
        self.ctx("user-a", PrincipalKind::Human, None)
    }
    fn agent(&self, who: &str) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, Some("project-a"))
            .with_session(id("session-a"))
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
    fn set(&self, key: &str, value: Value) {
        let key = key.to_owned();
        self.write(move |tx| {
            let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
            let schema = serde_json::to_value(descriptor(&key).unwrap().schema_ref()).unwrap();
            let a: SettingAssignment = serde_json::from_value(
                json!({"key": key, "value": {"schema": schema, "value": value}}),
            )
            .unwrap();
            let plan = model::plan(&current, &[a]);
            assert!(plan.is_valid());
            let body = model::body(
                &plan,
                &actor("user-a"),
                tx.now_ms(),
                json!({"kind": "test"}),
            );
            tx.configuration().append_revision(
                &ConfigScope::Workspace,
                model::NAMESPACE,
                current.revision,
                model::BODY_SCHEMA_VERSION,
                body,
            )?;
            Ok(())
        });
    }
    fn query_at(
        &self,
        ctx: &InvocationContext,
        op: &str,
        mut body: Value,
        revision: Option<u32>,
    ) -> DomainQueryReply {
        body["kind"] = json!(op);
        let q = format!("q-{}", self.n.fetch_add(1, Ordering::SeqCst));
        let mut frame =
            json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": q, "body": body});
        if let Some(r) = revision {
            frame["schemaRevision"] = json!(r);
        }
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
            .query(&gateway, ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn fetch_as(
        &self,
        ctx: &InvocationContext,
        url: &str,
        scope: Value,
        max: u32,
        ms: u64,
    ) -> DomainQueryReply {
        self.query_at(
            ctx,
            "web.fetch",
            json!({"scope": scope, "url": url, "maxBytes": max, "timeoutMs": ms.to_string()}),
            Some(2),
        )
    }
    fn fetch(&self, url: &str) -> DomainQueryReply {
        self.fetch_as(
            &self.user(),
            url,
            json!({"kind": "Project", "projectId": "project-a"}),
            65536,
            10_000,
        )
    }
    fn search_as(
        &self,
        ctx: &InvocationContext,
        query: &str,
        provider: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> DomainQueryReply {
        let mut page = json!({"limit": limit});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        self.query_at(
            ctx,
            "web.search",
            json!({"scope": {"kind": "Project", "projectId": "project-a"}, "query": query,
                "providerId": provider, "page": page}),
            Some(2),
        )
    }
    fn search(&self, query: &str, limit: u32, cursor: Option<&str>) -> DomainQueryReply {
        self.search_as(&self.user(), query, "brave", limit, cursor)
    }
    /// Every web diagnostic, oldest first.
    fn records(&self, component: &str) -> Vec<StoredDiagnostic> {
        let filter = DiagnosticFilter {
            component: Some(component.to_owned()),
            ..DiagnosticFilter::default()
        };
        self.read(move |s| s.diagnostics().diagnostics(&filter, 500))
    }
    /// The stored bytes of an artifact version.
    fn content(&self, doc: &WebDocument) -> String {
        let v = doc.content.version_id.clone();
        let version = self
            .read(move |s| s.artifacts().version(&v))
            .expect("version");
        let mut out = String::new();
        self.files
            .open(
                &version.blob_digest,
                &version.blob_generation,
                version.byte_count as u64,
            )
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        out
    }
    /// A local server reachable as `name` at the public address `public`.
    fn serve(&self, name: &str, public: &str, routes: Routes) -> Server {
        let server = Server::start(routes);
        self.names.set(name, &[&[public]]);
        self.net.map(public, server.addr);
        server
    }
}

fn document(reply: DomainQueryReply) -> WebDocument {
    match reply {
        DomainQueryReply::Result {
            result: OperationResult::WebFetch { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    }
}
fn results(reply: DomainQueryReply) -> Page<WebResult> {
    match reply {
        DomainQueryReply::Result {
            result: OperationResult::WebSearch { value },
            ..
        } => value,
        other => panic!("{other:?}"),
    }
}
fn refused(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
fn target(f: &DomainFault) -> String {
    f.target_kind
        .as_ref()
        .map(|t| t.0.clone())
        .unwrap_or_default()
}

const PAGE_HTML: &str = r#"<!doctype html><html><head><title>Rust Guide</title>
<script>steal(document.cookie)</script><style>p{}</style></head>
<body><h1>Install</h1><p>Run <code>cargo build</code> &amp; enjoy.</p>
<p>Example token ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4 in docs.</p></body></html>"#;

// ---------------------------------------------------------------------------------------------
// Fetch

#[test]
fn a_fetch_follows_checked_redirects_and_keeps_provenanced_untrusted_text() {
    let fx = Fx::new();
    let server = fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(|path| match path {
            "/start" => redirect("301 Moved Permanently", "/guide?token=t0psecret&lang=en"),
            p if p.starts_with("/guide") => response(
                "200 OK",
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Set-Cookie", "sid=1"),
                ],
                PAGE_HTML.as_bytes(),
            ),
            _ => response("404 Not Found", &[], b""),
        }),
    );
    let doc = document(fx.fetch("http://docs.test/start#frag"));
    assert_eq!(doc.url.0, "http://docs.test/start");
    assert_eq!(
        doc.final_url.0,
        "http://docs.test/guide?token=[REDACTED]&lang=en"
    );
    assert_eq!(doc.status, Some(200));
    assert!(matches!(doc.trust, Some(TrustLevel::Untrusted)));
    assert_eq!(doc.media_type.0, "text/html");
    assert!(!doc.truncated);
    let chain = doc.redirects.clone().unwrap().0;
    assert_eq!(chain.len(), 1);
    assert_eq!(
        (chain[0].url.0.as_str(), chain[0].status),
        ("http://docs.test/start", 301)
    );
    assert_eq!(
        String::from(doc.source_sha256.clone().unwrap()),
        sha256_hex(PAGE_HTML.as_bytes())
    );
    assert_eq!(
        doc.source_byte_length.as_ref().unwrap().as_str(),
        PAGE_HTML.len().to_string()
    );
    // The kept text: extracted, no script or style, the credential-shaped token redacted.
    let text = fx.content(&doc);
    assert!(
        text.contains("Install") && text.contains("Run cargo build & enjoy."),
        "{text}"
    );
    assert!(!text.contains("steal") && !text.contains("p{}"), "{text}");
    assert!(
        !text.contains("ghp_A1b2") && text.contains("[REDACTED]"),
        "{text}"
    );
    assert_eq!(doc.content.media_type, "text/plain; charset=utf-8");
    // The artifact is the project's, tagged untrusted, with its provenance and history.
    let a = doc.content.artifact_id.clone();
    let (artifact, tags) = fx.read(move |s| {
        Ok((
            s.artifacts().artifact(&a)?.unwrap(),
            s.artifacts().tags(&a)?,
        ))
    });
    assert_eq!(artifact.scope, ArtifactScope::Project(project("project-a")));
    assert_eq!(artifact.title, "Rust Guide");
    assert_eq!(artifact.provenance["trust"], "Untrusted");
    assert!(tags.contains(&"untrusted".to_owned()) && tags.contains(&"web.fetch".to_owned()));
    // Each hop resolved once and connected only to the checked address.
    assert_eq!(fx.names.calls("docs.test"), 2);
    let asked = fx.net.asked.lock().unwrap().clone();
    assert_eq!(asked, vec![SocketAddr::new(ip(PUBLIC), 80); 2]);
    // No cookie, credential or compression was requested; the Host is the canonical name.
    let seen = server.seen();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        let head = s.head.to_ascii_lowercase();
        assert!(head.contains("host: docs.test\r\n"), "{head}");
        assert!(head.contains("accept-encoding: identity"), "{head}");
        for absent in ["cookie", "authorization", "proxy-", "x-subscription-token"] {
            assert!(!head.contains(absent), "{absent} sent: {head}");
        }
    }
    assert!(fx.net.headers.lock().unwrap().iter().all(Vec::is_empty));
    // The activity is recorded with the evidence, the chain and every decision; no secret.
    let records = fx.records("web.fetch");
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.code, "fetched");
    assert!(
        r.output
            .as_deref()
            .unwrap()
            .contains("\"verdict\":\"allowed\"")
    );
    assert!(!format!("{r:?}").contains("t0psecret"));
    // A revision-1 caller receives exactly the revision-1 document.
    let r1 = fx.query_at(
        &fx.user(),
        "web.fetch",
        json!({"scope": {"kind": "Project", "projectId": "project-a"}, "url": "http://docs.test/guide", "maxBytes": 100, "timeoutMs": "5000"}),
        None,
    );
    let doc1 = document(r1);
    assert!(doc1.trust.is_none() && doc1.status.is_none() && doc1.redirects.is_none());
    assert!(doc1.truncated, "100 bytes of a longer page");
}

#[test]
fn loopback_private_link_local_and_metadata_destinations_are_refused_before_any_request() {
    let fx = Fx::new();
    let server = Server::start(Arc::new(|_| {
        response("200 OK", &[("Content-Type", "text/plain")], b"local secret")
    }));
    let port = server.addr.port();
    fx.names.set("internal.test", &[&["10.0.0.5"]]);
    fx.names.set("mixed.test", &[&[PUBLIC, "192.168.1.10"]]);
    fx.names.set("v6local.test", &[&["fd12::1"]]);
    let cases = [
        (
            format!("http://127.0.0.1:{port}/"),
            "web_destination_Loopback",
        ),
        (
            format!("http://2130706433:{port}/"),
            "web_destination_Loopback",
        ),
        (format!("http://0x7f.1:{port}/"), "web_destination_Loopback"),
        (format!("http://[::1]:{port}/"), "web_destination_Loopback"),
        (
            format!("http://[::ffff:127.0.0.1]:{port}/"),
            "web_destination_Loopback",
        ),
        ("http://0.0.0.0/".to_owned(), "web_destination_Unspecified"),
        (
            "http://169.254.169.254/latest/meta-data/".to_owned(),
            "web_destination_Metadata",
        ),
        (
            "http://[::ffff:a9fe:a9fe]/".to_owned(),
            "web_destination_Metadata",
        ),
        ("http://[fe80::1]/".to_owned(), "web_destination_LinkLocal"),
        ("http://localhost/".to_owned(), "web_destination_Loopback"),
        (
            "http://metadata.google.internal/".to_owned(),
            "web_destination_Metadata",
        ),
        (
            "http://internal.test/".to_owned(),
            "web_destination_Private",
        ),
        ("http://mixed.test/".to_owned(), "web_destination_Private"),
        (
            "http://v6local.test/".to_owned(),
            "web_destination_UniqueLocal",
        ),
    ];
    for (url, expected) in &cases {
        let f = refused(fx.fetch(url));
        assert_eq!(f.code, DomainErrorCode::PermissionDenied, "{url}");
        assert_eq!(&target(&f), expected, "{url}");
        let diagnostic = f.diagnostic_id.clone().expect("the refusal is recorded");
        let record = fx
            .read(move |s| s.diagnostics().diagnostic(&diagnostic))
            .unwrap();
        assert!(
            record.output.unwrap().contains("\"verdict\":\"blocked\""),
            "{url}"
        );
    }
    // No request reached any server, and localhost/metadata names were never resolved.
    assert!(server.seen().is_empty());
    assert!(fx.net.asked.lock().unwrap().is_empty());
    assert_eq!(
        fx.names.calls("localhost") + fx.names.calls("metadata.google.internal"),
        0
    );
    assert_eq!(fx.records("web.fetch").len(), cases.len());

    // Allowing private networks reaches loopback, never link-local or metadata.
    fx.set("web.fetchAllowPrivateNetwork", json!(true));
    let doc = document(fx.fetch(&format!("http://127.0.0.1:{port}/")));
    assert_eq!(fx.content(&doc), "local secret");
    for (url, expected) in [
        ("http://169.254.169.254/", "web_destination_Metadata"),
        ("http://[fd00:ec2::254]/", "web_destination_Metadata"),
        ("http://169.254.1.1/", "web_destination_LinkLocal"),
        ("http://0.0.0.0/", "web_destination_Unspecified"),
    ] {
        assert_eq!(target(&refused(fx.fetch(url))), expected, "{url}");
    }
}

#[test]
fn every_redirect_hop_is_rechecked_and_rebinding_cannot_reach_a_private_address() {
    let fx = Fx::new();
    let private = Server::start(Arc::new(|_| {
        response("200 OK", &[("Content-Type", "text/plain")], b"admin")
    }));
    let to_private = format!("http://127.0.0.1:{}/admin", private.addr.port());
    let start = fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(move |path| match path {
            "/to-private" => redirect("302 Found", &to_private),
            "/to-meta" => redirect("307 Temporary Redirect", "http://169.254.169.254/"),
            "/to-creds" => redirect("302 Found", "http://u:pw@docs.test/x"),
            "/to-file" => redirect("302 Found", "file:///etc/passwd"),
            "/to-rebind" => redirect("302 Found", "http://rebind.test/next"),
            _ => response("200 OK", &[("Content-Type", "text/plain")], b"public"),
        }),
    );
    let f = refused(fx.fetch("http://docs.test/to-private"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (
            DomainErrorCode::PermissionDenied,
            "web_destination_Loopback"
        )
    );
    let f = refused(fx.fetch("http://docs.test/to-meta"));
    assert_eq!(target(&f), "web_destination_Metadata");
    let f = refused(fx.fetch("http://docs.test/to-creds"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (
            DomainErrorCode::PermissionDenied,
            "web_redirect_url_credentials"
        )
    );
    let f = refused(fx.fetch("http://docs.test/to-file"));
    assert_eq!(target(&f), "web_redirect_url_scheme");
    assert!(
        private.seen().is_empty(),
        "the private server was never reached"
    );
    assert!(start.seen().iter().all(|s| s.path.starts_with("/to-")));
    // The redirect chain is part of a refusal's record; the credentials never are.
    let all: String = fx
        .records("web.fetch")
        .iter()
        .filter_map(|r| r.output.clone())
        .collect();
    assert!(
        all.contains("\"status\":302") && !all.contains("pw@"),
        "{all}"
    );

    // Rebinding: the first answer is public, every later one private. The fetch connects only to
    // the address it checked; the next hop's fresh answer is checked again and refused.
    fx.names.set("rebind.test", &[&[PUBLIC_2], &["127.0.0.1"]]);
    let rebinding = Server::start(Arc::new(|path| match path {
        "/first" => redirect("302 Found", "/second"),
        _ => response("200 OK", &[("Content-Type", "text/plain")], b"rebound"),
    }));
    fx.net.map(PUBLIC_2, rebinding.addr);
    let f = refused(fx.fetch("http://rebind.test/first"));
    assert_eq!(target(&f), "web_destination_Loopback");
    assert_eq!(
        rebinding.seen().len(),
        1,
        "only the checked public hop was sent"
    );
    assert!(
        !fx.net
            .asked
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.ip().is_loopback()),
        "the transport was never asked to connect to an unchecked address"
    );
}

#[test]
fn redirects_are_bounded_by_the_setting() {
    let fx = Fx::new();
    fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(|path| {
            let n: u32 = path.trim_start_matches("/r").parse().unwrap_or(0);
            redirect("302 Found", &format!("/r{}", n + 1))
        }),
    );
    fx.set("web.fetchMaxRedirects", json!(2));
    let f = refused(fx.fetch("http://docs.test/r0"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::ExternalFailure, "web_redirect_limit")
    );
    fx.set("web.fetchMaxRedirects", json!(0));
    assert_eq!(
        target(&refused(fx.fetch("http://docs.test/r0"))),
        "web_redirect_limit"
    );
}

#[test]
fn urls_carrying_credentials_are_refused_and_never_recorded() {
    let fx = Fx::new();
    let f = refused(fx.fetch("http://alice:hunter2pass@docs.test/"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::InvalidArgument, "web_url_credentials")
    );
    let all = format!("{:?}", fx.records("web.fetch"));
    assert!(
        !all.contains("hunter2pass") && !all.contains("alice"),
        "{all}"
    );
    assert_eq!(fx.names.calls("docs.test"), 0);
}

#[test]
fn content_types_limits_framing_and_statuses_are_handled() {
    let fx = Fx::new();
    let big = "x".repeat(5000);
    let big_body = big.clone();
    fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(move |path| match path {
            "/png" => response("200 OK", &[("Content-Type", "image/png")], b"\x89PNG"),
            "/gz" => response("200 OK", &[("Content-Type", "text/html"), ("Content-Encoding", "gzip")], b"\x1f\x8b"),
            "/json" => response("200 OK", &[("Content-Type", "application/json")], br#"{"a":1}"#),
            "/plain" => response("200 OK", &[("Content-Type", "text/plain; charset=iso-8859-1")], b"caf\xe9"),
            "/untyped-text" => response("200 OK", &[], b"just text"),
            "/untyped-binary" => response("200 OK", &[], b"\x00\x01\x02"),
            "/big" => response("200 OK", &[("Content-Type", "text/plain")], big_body.as_bytes()),
            "/chunked" => Reply::Bytes(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n".to_vec(),
            ),
            "/short" => Reply::Bytes(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 50\r\n\r\nonly part".to_vec(),
            ),
            "/missing" => response("404 Not Found", &[("Content-Type", "text/html")], b"<p>nope</p>"),
            _ => response("500 Internal Server Error", &[], b""),
        }),
    );
    let f = refused(fx.fetch("http://docs.test/png"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::UnsupportedTarget, "web_content_type")
    );
    assert_eq!(
        target(&refused(fx.fetch("http://docs.test/gz"))),
        "web_content_encoding"
    );
    assert_eq!(
        target(&refused(fx.fetch("http://docs.test/untyped-binary"))),
        "web_content_type"
    );
    let json = document(fx.fetch("http://docs.test/json"));
    assert_eq!(fx.content(&json), r#"{"a":1}"#);
    assert_eq!(json.content.media_type, "application/json");
    assert_eq!(
        fx.content(&document(fx.fetch("http://docs.test/plain"))),
        "café"
    );
    assert_eq!(
        fx.content(&document(fx.fetch("http://docs.test/untyped-text"))),
        "just text"
    );
    assert_eq!(
        fx.content(&document(fx.fetch("http://docs.test/chunked"))),
        "hello world"
    );
    // The caller's byte limit bounds what is kept, and says so.
    let cut = document(fx.fetch_as(
        &fx.user(),
        "http://docs.test/big",
        json!({"kind": "Project", "projectId": "project-a"}),
        1000,
        10_000,
    ));
    assert!(cut.truncated);
    assert_eq!(fx.content(&cut), big[..1000]);
    assert_eq!(cut.source_byte_length.unwrap().as_str(), "1000");
    // A body cut short after dispatch is an unknown outcome, never success.
    let f = refused(fx.fetch("http://docs.test/short"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::OutcomeUnknown, "web_incomplete_response")
    );
    let f = refused(fx.fetch("http://docs.test/missing"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::ExternalFailure, "web_http_status_404")
    );
}

#[test]
fn time_limits_cancellation_and_unknown_outcomes_are_explicit() {
    let fx = Fx::new();
    let server = fx.serve("slow.test", PUBLIC, Arc::new(|_| Reply::Hang));
    // Sent and unanswered within the caller's limit: unknown, with the dispatch recorded.
    let started = Instant::now();
    let f = refused(fx.fetch_as(
        &fx.user(),
        "http://slow.test/",
        json!({"kind": "Project", "projectId": "project-a"}),
        1000,
        1200,
    ));
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "bounded by the time limit"
    );
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::OutcomeUnknown, "web_timeout_after_send")
    );
    assert_eq!(server.seen().len(), 1);
    let diagnostic = f.diagnostic_id.clone().unwrap();
    let record = fx
        .read(move |s| s.diagnostics().diagnostic(&diagnostic))
        .unwrap();
    assert!(record.output.unwrap().contains("\"sent\":true"));
    // The setting caps a longer request.
    fx.set("web.timeoutMs", json!(1000));
    let started = Instant::now();
    let f = refused(fx.fetch_as(
        &fx.user(),
        "http://slow.test/",
        json!({"kind": "Project", "projectId": "project-a"}),
        1000,
        60_000,
    ));
    assert_eq!(f.code, DomainErrorCode::OutcomeUnknown);
    assert!(started.elapsed() < Duration::from_secs(5));
    // Nothing listening: not sent, an external failure.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    fx.names.set("down.test", &[&[PUBLIC_2]]);
    fx.net.map(PUBLIC_2, closed);
    let f = refused(fx.fetch("http://down.test/"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::ExternalFailure, "web_unreachable")
    );
    let f = refused(fx.fetch("http://unknown-name.test/"));
    assert_eq!(f.code, DomainErrorCode::ExternalFailure);
    // Shutdown stops a fetch in flight at its next wait, and refuses new ones.
    fx.set("web.timeoutMs", json!(30_000));
    let web = fx.web.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        web.shutdown();
    });
    let started = Instant::now();
    let f = refused(fx.fetch_as(
        &fx.user(),
        "http://slow.test/",
        json!({"kind": "Project", "projectId": "project-a"}),
        1000,
        30_000,
    ));
    stopper.join().unwrap();
    assert_eq!(f.code, DomainErrorCode::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        refused(fx.fetch("http://docs.test/")).code,
        DomainErrorCode::Cancelled
    );
}

#[test]
fn tool_switches_reviewers_and_scope_are_enforced_at_invocation() {
    let fx = Fx::new();
    fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(|_| response("200 OK", &[("Content-Type", "text/plain")], b"doc")),
    );
    let project_a = json!({"kind": "Project", "projectId": "project-a"});
    // A read-only code reviewer keeps the shared web reads, in its own project.
    let reviewer = fx.agent("reviewer-a");
    let doc = document(fx.fetch_as(
        &reviewer,
        "http://docs.test/",
        project_a.clone(),
        1000,
        5000,
    ));
    assert_eq!(fx.content(&doc), "doc");
    // Each tool has its own switch, enforced at invocation for agents.
    fx.set("tools.webFetch", json!(false));
    let f = refused(fx.fetch_as(
        &fx.agent("agent-a"),
        "http://docs.test/",
        project_a.clone(),
        1000,
        5000,
    ));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (
            DomainErrorCode::PermissionDenied,
            "authority:WebFetchDisabled"
        )
    );
    let f = refused(fx.fetch_as(
        &reviewer,
        "http://docs.test/",
        project_a.clone(),
        1000,
        5000,
    ));
    assert_eq!(target(&f), "authority:WebFetchDisabled");
    // The user's own UI action is not an agent tool.
    document(fx.fetch("http://docs.test/"));
    fx.set("tools.webFetch", json!(true));
    fx.set("tools.webSearch", json!(false));
    let f = refused(fx.search_as(&fx.agent("agent-a"), "rust", "brave", 5, None));
    assert_eq!(target(&f), "authority:WebToolsDisabled");
    document(fx.fetch_as(
        &fx.agent("agent-a"),
        "http://docs.test/",
        project_a,
        1000,
        5000,
    ));
    // A project-confined agent cannot fetch into another project or the workspace.
    let f = refused(fx.fetch_as(
        &fx.agent("agent-a"),
        "http://docs.test/",
        json!({"kind": "Project", "projectId": "project-b"}),
        1000,
        5000,
    ));
    assert_eq!(f.code, DomainErrorCode::PermissionDenied);
    let f = refused(fx.fetch_as(
        &fx.agent("agent-a"),
        "http://docs.test/",
        json!({"kind": "Workspace"}),
        1000,
        5000,
    ));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::PermissionDenied, "workspace_scope")
    );
    // The user's workspace fetch is a workspace artifact owned by the user.
    let doc = document(fx.fetch_as(
        &fx.user(),
        "http://docs.test/",
        json!({"kind": "Workspace"}),
        1000,
        5000,
    ));
    let a = doc.content.artifact_id.clone();
    let artifact = fx.read(move |s| s.artifacts().artifact(&a)).unwrap();
    assert_eq!(artifact.scope, ArtifactScope::Workspace);
    assert_eq!(artifact.owner, Some(actor("user-a")));
}

#[test]
fn a_full_project_store_refuses_the_content_and_resolves_its_staging() {
    let fx = Fx::new();
    fx.serve(
        "docs.test",
        PUBLIC,
        Arc::new(|_| response("200 OK", &[("Content-Type", "text/plain")], b"0123456789")),
    );
    let key = "storage.artifactQuotaBytes".to_owned();
    fx.write(move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let schema = serde_json::to_value(descriptor(&key).unwrap().schema_ref()).unwrap();
        let a: SettingAssignment =
            serde_json::from_value(json!({"key": key, "value": {"schema": schema, "value": 5}}))
                .unwrap();
        let plan = model::plan(&current, &[a]);
        let body = model::body(
            &plan,
            &actor("user-a"),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    });
    let f = refused(fx.fetch("http://docs.test/"));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::CapacityWait, "artifact_storage_quota")
    );
    assert!(f.diagnostic_id.is_some());
    let (versions, inflight) = fx.read(|s| {
        Ok((
            s.artifacts().project_usage(&project("project-a"))?,
            s.operations().unresolved(10)?.len(),
        ))
    });
    assert_eq!(
        (versions, inflight),
        (0, 0),
        "nothing kept and no staging left in flight"
    );
}

// ---------------------------------------------------------------------------------------------
// Search

fn brave(fx: &Fx, routes: Routes) -> Server {
    fx.serve("search.test", SEARCH_ADDR, routes)
}
fn configure_search(fx: &Fx) {
    fx.set("web.searchProvider", json!("brave"));
    let reference = SecretRef::mint(7);
    fx.set(
        "web.searchCredential",
        json!({"secretRef": reference.as_str()}),
    );
    fx.secrets
        .0
        .lock()
        .unwrap()
        .insert(reference.as_str().to_owned(), KEY.to_owned());
}
fn brave_answer(offset: usize, n: usize, more: bool) -> Reply {
    // The last result of every page has an unusable URL, which is left out.
    let results: Vec<Value> = (0..n)
        .map(|i| {
            let k = offset * 20 + i;
            let url = if i + 1 == n {
                "javascript:alert(1)".to_owned()
            } else {
                format!("https://r{k}.example/doc?api_key=abc{k}")
            };
            json!({"title": format!("<strong>Result</strong> {k}"), "url": url,
                "description": format!("About <em>rust</em> {k}")})
        })
        .collect();
    let body =
        json!({"query": {"more_results_available": more}, "web": {"results": results}}).to_string();
    response(
        "200 OK",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
}

#[test]
fn search_without_a_provider_or_credential_is_refused_never_empty() {
    let fx = Fx::new();
    let f = refused(fx.search("rust", 5, None));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (
            DomainErrorCode::UnsupportedCapability,
            "web_search_provider"
        )
    );
    assert!(f.diagnostic_id.is_some());
    fx.set("web.searchProvider", json!("brave"));
    let f = refused(fx.search("rust", 5, None));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (
            DomainErrorCode::CredentialsRequired,
            "web_search_credential"
        )
    );
    // A reference the secret store cannot resolve is still a missing credential.
    fx.set(
        "web.searchCredential",
        json!({"secretRef": SecretRef::mint(9).as_str()}),
    );
    let f = refused(fx.search("rust", 5, None));
    assert_eq!(f.code, DomainErrorCode::CredentialsRequired);
    // A provider the request does not name is not the configured one.
    configure_search(&fx);
    let f = refused(fx.search_as(&fx.user(), "rust", "google", 5, None));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::NotFound, "web_search_provider")
    );
    assert_eq!(fx.names.calls("search.test"), 0, "nothing was sent");
}

#[test]
fn search_pages_are_bounded_cursored_untrusted_and_recorded_without_the_key() {
    let fx = Fx::new();
    configure_search(&fx);
    let server = brave(
        &fx,
        Arc::new(|path| {
            let page: usize = path
                .split("offset=")
                .nth(1)
                .and_then(|s| s.split('&').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            brave_answer(page, 20, page < 2)
        }),
    );
    let first = results(fx.search("rust async", 3, None));
    assert_eq!(first.items.0.len(), 3);
    let r = &first.items.0[0];
    assert_eq!(r.title.0, "Result 0");
    assert_eq!(r.snippet.0, "About rust 0");
    assert_eq!(r.url.0, "https://r0.example/doc?api_key=[REDACTED]");
    assert!(matches!(r.trust, Some(TrustLevel::Untrusted)));
    assert_eq!(r.provider_id.as_ref().map(Id::as_str), Some("brave"));
    assert!(first.has_more);
    // The next page continues where the first ended.
    let cursor = first.next_cursor.clone().unwrap().0;
    let second = results(fx.search("rust async", 3, Some(&cursor)));
    assert_eq!(second.items.0[0].title.0, "Result 3");
    // A limit above the provider's page size is bounded by it, not truncated to one page.
    let wide = results(fx.search("rust async", 500, None));
    assert_eq!(wide.items.0.len(), 19, "the unusable URL is left out");
    let mut cursor = wide.next_cursor.clone();
    let mut pages = 1;
    while let Some(c) = cursor {
        let p = results(fx.search("rust async", 500, Some(&c.0)));
        pages += 1;
        cursor = p.next_cursor.clone();
        assert!(pages < 20);
    }
    assert_eq!(pages, 3, "every provider page is reachable");
    // The key went only to the provider, in its header; the query string carried the paging.
    let seen = server.seen();
    assert!(
        seen.iter()
            .all(|s| s.head.contains(&format!("X-Subscription-Token: {KEY}")))
    );
    assert!(seen[0].path.contains("q=rust+async") && seen[0].path.contains("count=20"));
    // Cursors are bound to the query and the configuration revision.
    let f = refused(fx.search(
        "other query",
        3,
        Some(&first.next_cursor.clone().unwrap().0),
    ));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    let f = refused(fx.search("rust async", 3, Some("v1.3132.deadbeefdeadbeefdeadbeef")));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    fx.set("web.timeoutMs", json!(20_000));
    let f = refused(fx.search("rust async", 3, Some(&first.next_cursor.unwrap().0)));
    assert_eq!(f.code, DomainErrorCode::CursorExpired);
    // Every search is recorded with its result URLs; neither the key nor the query text is.
    let records = fx.records("web.search");
    assert!(records.iter().all(|r| r.code == "searched"));
    let all = format!("{records:?}");
    assert!(all.contains("r0.example") && !all.contains(KEY) && !all.contains("rust async"));
}

#[test]
fn provider_failures_are_typed_and_unknown_after_dispatch() {
    let fx = Fx::new();
    configure_search(&fx);
    let status = Arc::new(Mutex::new("401 Unauthorized"));
    let s = status.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    brave(
        &fx,
        Arc::new(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
            match *s.lock().unwrap() {
                "hang" => Reply::Hang,
                "garbage" => response("200 OK", &[("Content-Type", "application/json")], b"<html>"),
                st => response(st, &[], format!("bad key {KEY}").as_bytes()),
            }
        }),
    );
    let f = refused(fx.search("q", 5, None));
    assert_eq!(f.code, DomainErrorCode::CredentialsRequired);
    *status.lock().unwrap() = "429 Too Many Requests";
    assert_eq!(
        refused(fx.search("q", 5, None)).code,
        DomainErrorCode::CapacityWait
    );
    *status.lock().unwrap() = "500 Internal Server Error";
    assert_eq!(
        target(&refused(fx.search("q", 5, None))),
        "web_search_rejected"
    );
    *status.lock().unwrap() = "garbage";
    assert_eq!(
        target(&refused(fx.search("q", 5, None))),
        "web_search_malformed"
    );
    *status.lock().unwrap() = "hang";
    fx.set("web.timeoutMs", json!(1000));
    let f = refused(fx.search("q", 5, None));
    assert_eq!(
        (f.code, target(&f).as_str()),
        (DomainErrorCode::OutcomeUnknown, "web_timeout_after_send")
    );
    assert_eq!(hits.load(Ordering::SeqCst), 5);
    assert!(!format!("{:?}", fx.records("web.search")).contains(KEY));
}

// ---------------------------------------------------------------------------------------------
// TLS

mod tls {
    use super::*;
    use devworks_core::ports::web::{Scheme, ServerName};
    use rustls::pki_types::PrivateKeyDer;
    use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

    /// A one-shot TLS server for `localhost` and `127.0.0.1` signed by a fresh CA; returns its
    /// address and the CA to trust.
    fn tls_server(body: &'static [u8]) -> (SocketAddr, RootCertStore) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca.self_signed(&ca_key).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
            .unwrap()
            .signed_by(&leaf_key, &ca, &ca_key)
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = Arc::new(
            ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf.der().clone()],
                    PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
                )
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let Ok(stream) = stream else { return };
                let conn = ServerConnection::new(config.clone()).unwrap();
                let mut tls = StreamOwned::new(conn, stream);
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut b) {
                        Ok(1) => head.push(b[0]),
                        _ => break,
                    }
                }
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(reply.as_bytes());
                let _ = tls.write_all(body);
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        let mut roots = RootCertStore::empty();
        roots.add(ca.der().clone()).unwrap();
        (addr, roots)
    }

    fn get(t: &NativeTransport, addr: SocketAddr, name: ServerName) -> Result<String, NetFault> {
        let host = "localhost".to_owned();
        let mut r = t.get(&HopRequest {
            scheme: Scheme::Https,
            host_header: &host,
            server_name: &name,
            target: "/",
            addr,
            deadline: Instant::now() + Duration::from_secs(10),
            cancel: Vec::new(),
            user_agent: "test",
            accept: "*/*",
            headers: &[],
        })?;
        let mut s = String::new();
        r.body.read_to_string(&mut s).unwrap();
        Ok(s)
    }

    #[test]
    fn https_is_verified_against_the_trusted_roots_and_the_server_name() {
        let (addr, roots) = tls_server(b"over tls");
        let trusting = NativeTransport::with_roots(roots);
        assert_eq!(
            get(&trusting, addr, ServerName::Dns("localhost".into())).unwrap(),
            "over tls"
        );
        // A name the certificate does not cover fails the handshake: nothing was sent.
        let err = get(&trusting, addr, ServerName::Dns("other.test".into())).unwrap_err();
        assert!(matches!(err, NetFault::NotSent { .. }), "{err:?}");
        assert!(!err.dispatched());
        // The bundled public roots do not trust the test CA.
        let (addr, _) = tls_server(b"x");
        let public = NativeTransport::new();
        let err = get(&public, addr, ServerName::Dns("localhost".into())).unwrap_err();
        assert!(matches!(err, NetFault::NotSent { .. }), "{err:?}");
    }
}

#[test]
fn the_transport_refuses_header_injection_and_oversized_heads() {
    let t = NativeTransport::new();
    let server = Server::start(Arc::new(|_| {
        let mut b = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..5000 {
            b.extend_from_slice(format!("X-Pad-{i}: {}\r\n", "y".repeat(20)).as_bytes());
        }
        b.extend_from_slice(b"\r\n");
        Reply::Bytes(b)
    }));
    let name = devworks_core::ports::web::ServerName::Dns("docs.test".into());
    let mk = |target: &'static str, host: &'static str| HopRequest {
        scheme: devworks_core::ports::web::Scheme::Http,
        host_header: host,
        server_name: &name,
        target,
        addr: server.addr,
        deadline: Instant::now() + Duration::from_secs(5),
        cancel: Vec::new(),
        user_agent: "test",
        accept: "*/*",
        headers: &[],
    };
    let err = t.get(&mk("/\r\nCookie: x", "docs.test")).unwrap_err();
    assert!(matches!(err, NetFault::NotSent { .. }));
    let err = t.get(&mk("/", "docs.test\r\nX: y")).unwrap_err();
    assert!(matches!(err, NetFault::NotSent { .. }));
    assert!(server.seen().is_empty());
    let err = t.get(&mk("/", "docs.test")).unwrap_err();
    assert!(matches!(err, NetFault::AfterSend { .. }), "{err:?}");
}
