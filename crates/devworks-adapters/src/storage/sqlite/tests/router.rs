//! The inference router end to end on the real SQLite store (DW-17): `router.catalog`,
//! `.route`, `.pin`, `.availability` and `.request_status` through the gateway and host
//! authority, with receipts, both availability schema revisions, durable round-robin cursors and
//! pins across restart, failover within the pool, effort and capability constraints, cooldown
//! waits, auxiliary routing and migration 13. Connections are a scripted directory; nothing
//! here contacts a provider or runs inference.
use super::{Harness, T0};
use crate::storage::sqlite::{MIGRATION_0013_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service as auth;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::router::{ConnectionDirectory, ConnectionState, ConnectionStatus};
use devworks_core::ports::storage::*;
use devworks_core::protocol::records::{
    EffortName, RouteAvailability, RouteChoice, RouteIntent, RouteModel, SettingAssignment,
    TierName,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationResult,
};
use devworks_core::protocol::support::Page;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, DomainErrorCode, DomainFault, Id, OperationState, ProjectId,
    RetryAdvice, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::router::{
    AuxiliaryRequest, InferenceRouter, Purpose, RequestAdmission, select::Unavailable,
};
use devworks_core::settings::catalog::{AgentRole, descriptor};
use devworks_core::settings::model;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
fn project(s: &str) -> ProjectId {
    ProjectId(id(s))
}

/// A scripted connection directory.
#[derive(Default)]
struct Directory(Mutex<Vec<ConnectionStatus>>);
impl Directory {
    fn set(&self, profile: &str, conn: &str, state: ConnectionState, discovered: Option<&[&str]>) {
        let mut all = self.0.lock().unwrap();
        all.retain(|c| c.id.as_str() != conn);
        all.push(ConnectionStatus {
            id: id(conn),
            profile: profile.into(),
            state,
            discovered: discovered.map(|d| d.iter().map(|s| (*s).to_owned()).collect()),
            allowed: Vec::new(),
            excluded: Vec::new(),
        });
    }
    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}
impl ConnectionDirectory for Directory {
    fn connections(&self, _: &dyn RouterRead) -> Result<Vec<ConnectionStatus>, StorageError> {
        Ok(self.0.lock().unwrap().clone())
    }
}
const CLAUDE: &str = "ClaudeSubscriptionMessages";
const CODEX: &str = "CodexSubscriptionResponses";

struct Fx {
    h: Harness,
    store: Arc<SqliteStore>,
    router: Arc<InferenceRouter>,
    dir: Arc<Directory>,
    cases: UseCases,
    n: Arc<AtomicU64>,
}

impl Fx {
    /// A store with both subscriptions connected, the user, two agents and project sessions.
    fn new() -> Self {
        let fx = Self::open(
            Harness::new(),
            Arc::new(Directory::default()),
            Arc::new(AtomicU64::new(0)),
        );
        fx.dir
            .set(CLAUDE, "conn-claude", ConnectionState::Connected, None);
        fx.dir
            .set(CODEX, "conn-codex", ConnectionState::Connected, None);
        fx.store
            .with_writer_connection(|c| {
                super::super::conn::db(c.execute_batch(
                    "INSERT INTO projects VALUES('project-a','A','Active',1,1,1);
                     INSERT INTO projects VALUES('project-b','B','Active',1,1,1);",
                ))
            })
            .unwrap();
        fx.write(|tx| {
            for (a, k) in [
                ("user-a", PrincipalKind::Human),
                ("agent-a", PrincipalKind::Agent),
                ("agent-b", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(a), k, a)?;
            }
            Ok(())
        });
        let user = fx.user();
        for (who, p) in [("agent-a", "project-a"), ("agent-b", "project-b")] {
            let by = user.clone();
            let binding = NewPrincipalBinding {
                actor: actor(who),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(project(p)),
                tool_families: None,
                bound_by: actor("user-a"),
            };
            fx.write(move |tx| {
                auth::bind_principal(tx, &by, binding, None)
                    .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
            });
        }
        for s in ["s1", "s2", "s3", "s4"] {
            fx.session(s, "agent-a", None);
        }
        fx.session("work-a", "agent-a", Some("project-a"));
        fx.session("work-b", "agent-b", Some("project-b"));
        fx
    }
    fn open(h: Harness, dir: Arc<Directory>, n: Arc<AtomicU64>) -> Self {
        let store = Arc::new(h.store());
        let router = Arc::new(InferenceRouter::new(
            h.ids.clone(),
            h.clock.clone(),
            dir.clone(),
        ));
        let cases = UseCases::new().with_router(router.clone());
        Self {
            h,
            store,
            router,
            dir,
            cases,
            n,
        }
    }
    /// A process restart: every handle is dropped, then the store is reopened (a new boot).
    fn restart(self) -> Self {
        let Fx {
            h,
            store,
            router,
            dir,
            cases,
            n,
        } = self;
        drop((cases, router, store));
        Self::open(h, dir, n)
    }
    fn session(&self, s: &str, agent: &str, p: Option<&str>) {
        let (s, agent, p) = (id(s), actor(agent), p.map(project));
        self.write(move |tx| {
            tx.sessions().insert_session(&StoredSession {
                conversation_id: id(&format!("conversation-{}", s.as_str())),
                id: s,
                agent_id: agent.clone(),
                project_id: p,
                root_id: None,
                title: "Chat".into(),
                context_epoch: 0,
                revision: 1,
                created_by: agent,
                created_at_ms: T0,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
        });
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
    fn agent(&self, who: &str, p: &str, session: &str) -> InvocationContext {
        self.ctx(who, PrincipalKind::Agent, Some(p))
            .with_session(id(session))
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
    ) -> DomainCommandReply {
        body["kind"] = json!(op);
        let frame =
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body});
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
    fn command(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainCommandReply {
        let key = format!("key-{}", self.n.fetch_add(1, Ordering::SeqCst));
        self.command_keyed(ctx, op, &key, body)
    }
    fn query_at(
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

    // Router helpers

    fn route_as(
        &self,
        ctx: &InvocationContext,
        session: &str,
        intent: Value,
        effort: &str,
        caps: &[&str],
    ) -> DomainCommandReply {
        self.command(
            ctx,
            "router.route",
            json!({"sessionId": session, "expectedContextEpoch": "0", "intent": intent,
                "effort": effort, "requiredCapabilities": caps}),
        )
    }
    fn route(
        &self,
        session: &str,
        intent: Value,
        effort: &str,
        caps: &[&str],
    ) -> DomainCommandReply {
        self.route_as(&self.user(), session, intent, effort, caps)
    }
    fn pin(&self, session: &str, expected: i64, intent: Value, effort: &str) -> DomainCommandReply {
        self.command(
            &self.user(),
            "router.pin",
            json!({"sessionId": session, "expectedPinRevision": expected.to_string(),
                "intent": intent, "effort": effort}),
        )
    }
    fn availability(
        &self,
        intent: Value,
        effort: &str,
        caps: &[&str],
        session: Option<&str>,
    ) -> RouteAvailability {
        let mut body = json!({"intent": intent, "effort": effort, "requiredCapabilities": caps});
        if let Some(s) = session {
            body["sessionId"] = json!(s);
        }
        match result(self.query_at(&self.user(), "router.availability", body, Some(2))) {
            OperationResult::RouterAvailability { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn catalog(
        &self,
        include_disabled: bool,
        limit: u32,
        cursor: Option<&str>,
    ) -> Page<RouteModel> {
        let mut page = json!({"limit": limit});
        if let Some(c) = cursor {
            page["cursor"] = json!(c);
        }
        match result(self.query_at(
            &self.user(),
            "router.catalog",
            json!({"page": page, "includeDisabled": include_disabled}),
            None,
        )) {
            OperationResult::RouterCatalog { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn pin_row(&self, session: &str) -> Option<StoredPin> {
        let s = id(session);
        self.read(|r| r.router().session_pin(&s))
    }
    fn cursor(&self, tier: &str) -> i64 {
        let t = tier.to_owned();
        self.read(move |r| r.router().tier_cursor(&t))
    }
    fn cool(&self, model: &str, until: i64) {
        let (router, m) = (self.router.clone(), id(model));
        self.write(move |tx| router.record_cooldown(tx, &m, until, "RateLimited"));
    }
    fn uncool(&self, model: &str) {
        let m = id(model);
        self.write(move |tx| tx.router().clear_cooldown(&m));
    }
    fn set_members(&self, tier: &str, members: &[&str]) {
        let key = format!("routing.tier{tier}.members");
        let members: Vec<String> = members.iter().map(|m| (*m).to_owned()).collect();
        self.write(move |tx| {
            let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
            let schema = serde_json::to_value(descriptor(&key).unwrap().schema_ref()).unwrap();
            let a: SettingAssignment = serde_json::from_value(
                json!({"key": key, "value": {"schema": schema, "value": members}}),
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
    fn add_model(&self, m: StoredRouteModel) {
        self.write(move |tx| tx.router().put_route_model(&m, None));
    }
}

fn custom(model_id: &str, profile: &str, vision: bool, context: Option<i64>) -> StoredRouteModel {
    StoredRouteModel {
        id: id(model_id),
        display_name: format!("Custom {model_id}"),
        endpoint_profile: profile.into(),
        upstream_model_id: format!("upstream/{model_id}"),
        enabled: true,
        efforts: vec!["Low".into(), "High".into()],
        image_input: vision,
        tools: true,
        context_tokens: context,
        connection_id: None,
        position: 1_000,
        revision: 1,
        updated_at_ms: T0,
    }
}
fn tier(t: &str) -> Value {
    json!({"kind": "Tier", "tier": t})
}
fn named(m: &str) -> Value {
    json!({"kind": "Model", "modelId": m})
}
fn auto() -> Value {
    json!({"kind": "OpenRouterAuto"})
}
fn recorded(reply: DomainCommandReply) -> (DomainDecision, bool) {
    match reply {
        CommandReply::Recorded {
            receipt, replayed, ..
        } => (receipt.decision, replayed),
        other => panic!("expected a recorded decision, got {other:?}"),
    }
}
/// The choice and whether the decision wrote a pin (`Applied`) or reused one (`NoChange`).
fn chosen(reply: DomainCommandReply) -> (RouteChoice, bool) {
    match recorded(reply).0 {
        DomainDecision::Applied {
            result: OperationResult::RouterRoute { value } | OperationResult::RouterPin { value },
        } => (value, true),
        DomainDecision::NoChange {
            result: OperationResult::RouterRoute { value } | OperationResult::RouterPin { value },
        } => (value, false),
        other => panic!("expected a route choice, got {other:?}"),
    }
}
fn model_of(reply: DomainCommandReply) -> String {
    chosen(reply).0.model_id.as_str().to_owned()
}
fn refused(reply: DomainCommandReply) -> DomainFault {
    match recorded(reply).0 {
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
fn query_fault(reply: DomainQueryReply) -> DomainFault {
    match reply {
        DomainQueryReply::Rejected { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}
fn codes(a: &RouteAvailability) -> Vec<&str> {
    a.blocked_reason_codes
        .0
        .iter()
        .map(|c| c.0.as_str())
        .collect()
}
fn ids(a: &RouteAvailability) -> Vec<&str> {
    a.eligible_model_ids.0.iter().map(Id::as_str).collect()
}
fn effort(e: &EffortName) -> String {
    serde_json::to_value(e)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn catalog_lists_the_seeded_models_in_selector_order_with_tier_memberships() {
    let fx = Fx::new();
    let all = fx.catalog(false, 50, None);
    let names: Vec<&str> = all.items.0.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        names,
        [
            "openrouter-auto",
            "fable",
            "opus",
            "sonnet",
            "haiku",
            "astra",
            "sol",
            "terra",
            "luna"
        ]
    );
    let tiers = |m: &str| -> Vec<String> {
        let row = all.items.0.iter().find(|r| r.id.as_str() == m).unwrap();
        row.tier_memberships
            .0
            .iter()
            .map(|t| {
                serde_json::to_value(t)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    // ROUTE-19 initial membership, from the routing settings.
    for (m, t) in [
        ("fable", "Max"),
        ("astra", "Max"),
        ("opus", "High"),
        ("sol", "High"),
        ("sonnet", "Mid"),
        ("terra", "Mid"),
        ("haiku", "Low"),
        ("luna", "Low"),
    ] {
        assert_eq!(tiers(m), [t], "{m}");
    }
    assert!(
        tiers("openrouter-auto").is_empty(),
        "Open Router Auto is not a tier member"
    );
    // Paging continues in order; a disabled model is listed only on request.
    let first = fx.catalog(false, 4, None);
    assert!(first.has_more);
    let rest = fx.catalog(false, 50, first.next_cursor.as_ref().map(|c| c.0.as_str()));
    let paged: Vec<&str> = first
        .items
        .0
        .iter()
        .chain(rest.items.0.iter())
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(paged, names);
    fx.write(|tx| {
        let mut m = tx.router().route_model(&id("terra"))?.unwrap();
        m.enabled = false;
        let r = m.revision;
        tx.router().put_route_model(&m, Some(r))
    });
    assert!(
        !fx.catalog(false, 50, None)
            .items
            .0
            .iter()
            .any(|m| m.id.as_str() == "terra")
    );
    let disabled = fx.catalog(true, 50, None);
    let terra = disabled
        .items
        .0
        .iter()
        .find(|m| m.id.as_str() == "terra")
        .unwrap();
    assert!(!terra.enabled);
    assert_eq!(terra.revision.get(), 2);
    // A custom model assigned to a tier reports the membership.
    fx.add_model(custom("local-qwen", "CompatibleChat", false, Some(131_072)));
    fx.set_members("Low", &["haiku", "luna", "local-qwen"]);
    let custom_row = fx.catalog(false, 50, None);
    let row = custom_row
        .items
        .0
        .iter()
        .find(|m| m.id.as_str() == "local-qwen")
        .unwrap();
    assert!(matches!(row.tier_memberships.0.as_slice(), [TierName::Low]));
}

#[test]
fn tiers_allocate_round_robin_and_reuse_durable_pins() {
    let fx = Fx::new();
    let (a, wrote) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert!(wrote);
    assert_eq!(a.model_id.as_str(), "opus");
    assert_eq!(
        model_of(fx.route("s2", tier("High"), "Default", &[])),
        "sol"
    );
    assert_eq!(
        model_of(fx.route("s3", tier("High"), "Default", &[])),
        "opus"
    );
    assert_eq!(fx.cursor("High"), 3);
    // Reuse across turns: no write, no turn, the same pin.
    let (again, wrote) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert!(!wrote);
    assert_eq!(again.pin_id, a.pin_id);
    assert_eq!(again.pin_revision.get(), 1);
    assert_eq!(fx.cursor("High"), 3);
    // Each tier has its own cursor; a tier never hands out another tier's model.
    assert_eq!(
        model_of(fx.route("s4", tier("Max"), "Default", &[])),
        "fable"
    );
    assert_eq!(fx.cursor("Max"), 1);
    let pin = fx.pin_row("s1").unwrap();
    assert_eq!(pin.reason, "Initial");
    assert_eq!(pin.connection_id.as_str(), "conn-claude");
    // The pin is a history fact attributed to the session.
    let events = fx
        .read(|r| {
            r.history().page(
                &EventFilter::Aggregate {
                    kind: "route_pin".into(),
                    id: "s1".into(),
                },
                0,
                10,
            )
        })
        .events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "RoutePinned");
    assert_eq!(
        events[0].actor.0.as_str(),
        "user-a",
        "attributed to who routed it"
    );
    assert_eq!(events[0].payload["reason"], "Initial");
}

#[test]
fn named_models_are_direct_and_wait_instead_of_failing_over() {
    let fx = Fx::new();
    // Sonnet is a Mid member, but naming it routes to it without touching the Mid cursor.
    let (c, _) = chosen(fx.route("s1", named("sonnet"), "Default", &[]));
    assert_eq!(c.model_id.as_str(), "sonnet");
    assert!(matches!(c.intent, RouteIntent::Model { .. }));
    assert_eq!(fx.cursor("Mid"), 0);
    // Cooling: the request waits for Sonnet; it is not translated to Terra.
    fx.cool("sonnet", T0 + 60_000);
    let fault = refused(fx.route("s1", named("sonnet"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::CapacityWait);
    assert_eq!(fault.retry, RetryAdvice::AfterBlockerClears);
    assert_eq!(
        fx.pin_row("s1").unwrap().model_id.as_str(),
        "sonnet",
        "the pin is untouched"
    );
    let wait = fx.availability(named("sonnet"), "Default", &[], None);
    assert!(!wait.available);
    assert_eq!(
        wait.retry_at_ms.as_ref().map(Counter::get),
        Some(T0 + 60_000)
    );
    assert_eq!(codes(&wait), ["Cooling"]);
    // An unknown model is not found, never substituted.
    let fault = refused(fx.route("s2", named("gpt-unknown"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
}

#[test]
fn open_router_auto_is_its_own_upstream_route() {
    let fx = Fx::new();
    // No OpenRouter connection: a truthful credential blocker, not a DevWorks tier.
    let fault = refused(fx.route("s1", auto(), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::CredentialsRequired);
    assert_eq!(
        codes(&fx.availability(auto(), "Default", &[], None)),
        ["NoConnection"]
    );
    fx.dir.set(
        "OpenRouterChat",
        "conn-or",
        ConnectionState::Connected,
        None,
    );
    let (c, _) = chosen(fx.route("s1", auto(), "Default", &[]));
    assert_eq!(c.model_id.as_str(), "openrouter-auto");
    assert_eq!(c.connection_id.as_str(), "conn-or");
    assert!(matches!(c.intent, RouteIntent::OpenRouterAuto {}));
    // Tiers never allocate it.
    for s in ["s2", "s3", "s4"] {
        assert_ne!(
            model_of(fx.route(s, tier("High"), "Default", &[])),
            "openrouter-auto"
        );
    }
}

#[test]
fn failover_repins_within_the_pool_and_keeps_the_replacement_after_recovery() {
    let fx = Fx::new();
    let (first, _) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert_eq!(first.model_id.as_str(), "opus");
    fx.cool("opus", T0 + 30_000);
    let (second, wrote) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert!(wrote);
    assert_eq!(second.model_id.as_str(), "sol");
    assert_eq!(second.replaced_pin_id.as_ref(), Some(&first.pin_id));
    assert_ne!(second.pin_id, first.pin_id);
    assert_eq!(second.pin_revision.get(), 2);
    assert_eq!(fx.pin_row("s1").unwrap().reason, "Failover");
    // The original recovering does not move the session back.
    fx.uncool("opus");
    let (third, wrote) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert!(!wrote);
    assert_eq!(third.model_id.as_str(), "sol");
    // Removing the pinned model from the tier (membership is configuration) also fails over.
    fx.set_members("High", &["opus"]);
    assert_eq!(
        model_of(fx.route("s1", tier("High"), "Default", &[])),
        "opus"
    );
    // A lost connection is a failover too; a pinned model whose connection changed keeps its
    // model and records the connection change.
    fx.dir.set(
        CLAUDE,
        "conn-claude",
        ConnectionState::ReauthenticationRequired,
        None,
    );
    fx.dir
        .set(CLAUDE, "conn-claude-2", ConnectionState::Connected, None);
    let (moved, wrote) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert!(wrote);
    assert_eq!(moved.model_id.as_str(), "opus");
    assert_eq!(moved.connection_id.as_str(), "conn-claude-2");
    assert_eq!(fx.pin_row("s1").unwrap().reason, "ConnectionChanged");
}

#[test]
fn an_all_cooling_pool_waits_for_the_earliest_cooldown_and_follows_changes() {
    let fx = Fx::new();
    chosen(fx.route("s1", tier("High"), "Default", &[]));
    fx.cool("opus", T0 + 100_000);
    fx.cool("sol", T0 + 50_000);
    let fault = refused(fx.route("s1", tier("High"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::CapacityWait);
    // Never silently another tier, even with Mid available.
    assert!(fx.availability(tier("Mid"), "Default", &[], None).available);
    assert_eq!(fx.pin_row("s1").unwrap().model_id.as_str(), "opus");
    assert_eq!(fx.pin_row("s1").unwrap().revision, 1);
    let wait = fx.availability(tier("High"), "Default", &[], None);
    assert!(!wait.available);
    assert!(wait.eligible_model_ids.0.is_empty());
    assert_eq!(
        wait.retry_at_ms.as_ref().map(Counter::get),
        Some(T0 + 50_000)
    );
    // A later report extends; a shorter one never shortens a running cooldown.
    fx.cool("sol", T0 + 200_000);
    fx.cool("sol", T0 + 10_000);
    assert_eq!(
        fx.availability(tier("High"), "Default", &[], None)
            .retry_at_ms
            .map(|c| c.get()),
        Some(T0 + 100_000),
        "the earliest remaining expiry after the change"
    );
    // A past expiry records nothing; an expired cooldown no longer blocks.
    fx.cool("opus", T0 - 1);
    fx.h.clock.0.store(T0 + 100_001, Ordering::SeqCst);
    let after = fx.availability(tier("High"), "Default", &[], None);
    assert!(after.available);
    assert_eq!(ids(&after), ["opus"]);
    assert_eq!(after.retry_at_ms, None);
    // A member blocked by something besides its cooldown does not set the wait.
    fx.dir
        .set(CLAUDE, "conn-claude", ConnectionState::Disconnected, None);
    let mixed = fx.availability(tier("High"), "Default", &[], None);
    assert!(!mixed.available);
    assert_eq!(
        mixed.retry_at_ms.as_ref().map(Counter::get),
        Some(T0 + 200_000)
    );
    assert_eq!(codes(&mixed), ["Cooling", "CredentialsRequired"]);
    // Cleared after success: honoured at once.
    fx.uncool("sol");
    assert_eq!(
        ids(&fx.availability(tier("High"), "Default", &[], None)),
        ["sol"]
    );
}

#[test]
fn explicit_effort_constrains_eligibility_and_survives_failover() {
    let fx = Fx::new();
    // No High member declares Minimal: refused, never sent as a neighbouring level.
    let fault = refused(fx.route("s1", tier("High"), "Minimal", &[]));
    assert_eq!(fault.code, DomainErrorCode::UnsupportedCapability);
    assert_eq!(
        codes(&fx.availability(tier("High"), "Minimal", &[], None)),
        ["EffortUnsupported"]
    );
    // Max: only Astra declares Minimal.
    let (c, _) = chosen(fx.route("s1", tier("Max"), "Minimal", &[]));
    assert_eq!(c.model_id.as_str(), "astra");
    assert_eq!(effort(&c.requested_effort), "Minimal");
    assert_eq!(effort(&c.effective_effort), "Minimal");
    // XHigh in High: Opus declares it too since migration 15 corrected the Claude seed, so
    // narrow the pool to show a member without XHigh is never a replacement.
    fx.add_model(custom("no-xhigh", CLAUDE, true, None));
    fx.set_members("High", &["sol", "no-xhigh"]);
    // XHigh in High is Sol only; when Sol cools, a member without XHigh is not a replacement.
    let (x, _) = chosen(fx.route("s2", tier("High"), "XHigh", &[]));
    assert_eq!(x.model_id.as_str(), "sol");
    fx.cool("sol", T0 + 5_000);
    let fault = refused(fx.route("s2", tier("High"), "XHigh", &[]));
    assert_eq!(fault.code, DomainErrorCode::CapacityWait);
    assert_eq!(fx.pin_row("s2").unwrap().effective_effort, "XHigh");
    // An effort change the pinned model declares keeps the model and the pin ID.
    fx.uncool("sol");
    let (low, wrote) = chosen(fx.route("s2", tier("High"), "Low", &[]));
    assert!(wrote);
    assert_eq!(low.model_id.as_str(), "sol");
    assert_eq!(low.pin_id, x.pin_id);
    assert_eq!(low.replaced_pin_id, None);
    assert_eq!(fx.pin_row("s2").unwrap().reason, "EffortChanged");
    // Provider Default is a separate choice: always eligible, sent as the provider's default.
    let (d, _) = chosen(fx.route("s3", tier("Low"), "Default", &[]));
    assert_eq!(effort(&d.effective_effort), "Default");
    // The store refuses a pin whose explicit effort changed.
    let mut bad = fx.pin_row("s2").unwrap();
    bad.effective_effort = "High".into();
    let r = bad.revision;
    let err = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.router().put_session_pin(&bad, Some(r))
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::InvalidRecord(_)), "{err:?}");
}

#[test]
fn required_capabilities_are_enforced_and_never_ignored() {
    let fx = Fx::new();
    fx.add_model(custom("plain", CLAUDE, false, None));
    fx.add_model(custom("wide", CLAUDE, true, Some(400_000)));
    fx.set_members("High", &["plain", "wide"]);
    // Vision (ROUTE-13): only a model declaring image input.
    assert_eq!(
        model_of(fx.route("s1", tier("High"), "Default", &["vision"])),
        "wide"
    );
    assert_eq!(
        codes(&fx.availability(tier("High"), "Default", &["vision"], None)),
        ["VisionUnsupported"]
    );
    // Context: an unknown window never fits a stated requirement.
    let a = fx.availability(tier("High"), "Default", &["context:200000"], None);
    assert_eq!(ids(&a), ["wide"]);
    assert_eq!(codes(&a), ["ContextUnknown"]);
    let a = fx.availability(tier("High"), "Default", &["context:500000"], None);
    assert!(!a.available);
    assert_eq!(codes(&a), ["ContextTooSmall", "ContextUnknown"]);
    // The pin records what the request required; vision is never silently dropped on failover.
    assert_eq!(fx.pin_row("s1").unwrap().required_capabilities, ["vision"]);
    fx.cool("wide", T0 + 1_000);
    let fault = refused(fx.route("s1", tier("High"), "Default", &["vision"]));
    assert_eq!(fault.code, DomainErrorCode::CapacityWait);
    // A member that could not serve the request anyway never shortens the wait.
    fx.cool("plain", T0 + 500);
    let wait = fx.availability(tier("High"), "Default", &["vision"], None);
    assert_eq!(
        wait.retry_at_ms.as_ref().map(Counter::get),
        Some(T0 + 1_000)
    );
    // An unknown capability is refused, not ignored.
    let fault = refused(fx.route("s2", tier("High"), "Default", &["telepathy"]));
    assert_eq!(fault.code, DomainErrorCode::InvalidArgument);
    let fault = query_fault(fx.query_at(
        &fx.user(),
        "router.availability",
        json!({"intent": tier("High"), "effort": "Default", "requiredCapabilities": ["context:0"]}),
        None,
    ));
    assert_eq!(fault.code, DomainErrorCode::InvalidArgument);
}

#[test]
fn custom_members_join_the_full_tier_pool() {
    let fx = Fx::new();
    fx.add_model(custom("local-qwen", "CompatibleChat", false, Some(131_072)));
    fx.set_members("Low", &["haiku", "luna", "local-qwen"]);
    // Only the local endpoint is connected: the custom member serves the tier.
    fx.dir.clear();
    fx.dir.set(
        "CompatibleChat",
        "conn-local",
        ConnectionState::Connected,
        None,
    );
    assert_eq!(
        model_of(fx.route("s1", tier("Low"), "Default", &[])),
        "local-qwen"
    );
    // With everything connected, the full pool rotates (no hidden local-first subset).
    fx.dir
        .set(CLAUDE, "conn-claude", ConnectionState::Connected, None);
    fx.dir
        .set(CODEX, "conn-codex", ConnectionState::Connected, None);
    let got: Vec<String> = ["s2", "s3", "s4"]
        .iter()
        .map(|s| model_of(fx.route(s, tier("Low"), "Default", &[])))
        .collect();
    assert_eq!(
        got,
        ["luna", "local-qwen", "haiku"],
        "cursor 1, 2, 3 over three members"
    );
    // Discovery that does not list a model makes it ineligible.
    fx.dir.set(
        CODEX,
        "conn-codex",
        ConnectionState::Connected,
        Some(&["gpt-6.1-sol"]),
    );
    let a = fx.availability(tier("Low"), "Default", &[], None);
    assert_eq!(ids(&a), ["haiku", "local-qwen"]);
    assert_eq!(codes(&a), ["NotDiscovered"]);
    // The store refuses a decision model in the conversational catalog.
    let err = run_write(&*fx.store, WriteOptions::default(), |tx| {
        tx.router()
            .put_route_model(&custom("jev", "SystemOne", false, None), None)
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::InvalidRecord(_)), "{err:?}");
}

#[test]
fn concurrent_first_routes_share_one_pin_and_one_cursor_turn() {
    let fx = Arc::new(Fx::new());
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let fx = fx.clone();
            std::thread::spawn(move || chosen(fx.route("s1", tier("High"), "Default", &[])))
        })
        .collect();
    let results: Vec<(RouteChoice, bool)> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let pins: BTreeSet<&str> = results.iter().map(|(c, _)| c.pin_id.as_str()).collect();
    assert_eq!(pins.len(), 1, "one pin");
    assert_eq!(
        results.iter().filter(|(_, wrote)| *wrote).count(),
        1,
        "one allocation"
    );
    assert_eq!(fx.cursor("High"), 1, "one round-robin turn");
    // Concurrent first routes of different sessions take distinct consecutive turns.
    let handles: Vec<_> = ["s2", "s3", "s4"]
        .into_iter()
        .map(|s| {
            let fx = fx.clone();
            std::thread::spawn(move || model_of(fx.route(s, tier("High"), "Default", &[])))
        })
        .collect();
    let mut got: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    got.sort();
    assert_eq!(got, ["opus", "sol", "sol"]);
    assert_eq!(fx.cursor("High"), 4);
}

#[test]
fn a_replayed_route_returns_its_recorded_choice() {
    let fx = Fx::new();
    let body = json!({"sessionId": "s1", "expectedContextEpoch": "0", "intent": tier("High"),
        "effort": "Default", "requiredCapabilities": []});
    let first = chosen(fx.command_keyed(&fx.user(), "router.route", "route-1", body.clone())).0;
    // The pin changes afterwards (failover) ...
    fx.cool("opus", T0 + 9_000);
    assert_eq!(
        model_of(fx.route("s1", tier("High"), "Default", &[])),
        "sol"
    );
    // ... but a retry of the first request replays its receipt instead of routing again.
    let (decision, replayed) =
        recorded(fx.command_keyed(&fx.user(), "router.route", "route-1", body));
    assert!(replayed);
    let DomainDecision::Applied {
        result: OperationResult::RouterRoute { value },
    } = decision
    else {
        panic!("{decision:?}")
    };
    assert_eq!(value.model_id.as_str(), first.model_id.as_str());
    assert_eq!(value.pin_id, first.pin_id);
    assert_eq!(fx.cursor("High"), 2, "the replay took no turn");
}

#[test]
fn pins_and_cursors_survive_a_restart() {
    let fx = Fx::new();
    let (before, _) = chosen(fx.route("s1", tier("Mid"), "High", &[]));
    assert_eq!(before.model_id.as_str(), "sonnet");
    let fx = fx.restart();
    let (after, wrote) = chosen(fx.route("s1", tier("Mid"), "High", &[]));
    assert!(!wrote, "restored pin is reused");
    assert_eq!(after.pin_id, before.pin_id);
    assert_eq!(effort(&after.requested_effort), "High");
    // The cursor continues where it stopped.
    assert_eq!(model_of(fx.route("s2", tier("Mid"), "High", &[])), "terra");
    assert_eq!(fx.cursor("Mid"), 2);
}

#[test]
fn an_explicit_pin_change_persists_and_auto_restores_router_selection() {
    let fx = Fx::new();
    let (c, _) = chosen(fx.route("s1", tier("High"), "Default", &["vision"]));
    assert_eq!(c.model_id.as_str(), "opus");
    // A stale expected revision is a durable revision conflict.
    let fault = refused(fx.pin("s1", 7, named("haiku"), "Default"));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    assert_eq!(fault.current_revision.as_ref().map(Revision::get), Some(1));
    // Manual selection persists (and is direct).
    let (m, wrote) = chosen(fx.pin("s1", 1, named("haiku"), "Default"));
    assert!(wrote);
    assert_eq!(m.model_id.as_str(), "haiku");
    assert_eq!(fx.pin_row("s1").unwrap().reason, "SelectionChanged");
    let (kept, wrote) = chosen(fx.route("s1", named("haiku"), "Default", &["vision"]));
    assert!(!wrote);
    assert_eq!(kept.pin_id, m.pin_id);
    // An unchanged explicit selection is NoChange.
    let (same, wrote) = chosen(fx.pin("s1", 2, named("haiku"), "Default"));
    assert!(!wrote);
    assert_eq!(same.pin_revision.get(), 2);
    // Back to a tier: router selection with pinning (a fresh round-robin turn).
    let (t, _) = chosen(fx.pin("s1", 2, tier("High"), "Default"));
    assert_eq!(t.model_id.as_str(), "sol", "cursor turn 1");
    assert!(matches!(t.intent, RouteIntent::Tier { .. }));
    // The capabilities of the session's last request still apply to an explicit change.
    fx.add_model(custom("plain", CLAUDE, false, None));
    let fault = refused(fx.pin("s1", 3, named("plain"), "Default"));
    assert_eq!(fault.code, DomainErrorCode::UnsupportedCapability);
    // No pin yet: nothing to change.
    let fault = refused(fx.pin("s2", 1, named("haiku"), "Default"));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
}

#[test]
fn auxiliary_requests_never_repin_or_take_a_turn() {
    let fx = Fx::new();
    let (c, _) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    let aux = |purpose| AuxiliaryRequest {
        purpose,
        session_id: Some(id("s1")),
        intent: RouteIntent::Tier {
            tier: TierName::Low,
        },
        effort: EffortName::Default,
        required_capabilities: Vec::new(),
    };
    // Compaction reuses the eligible pin.
    let r = fx
        .read(|s| fx.router.route_auxiliary(s, &aux(Purpose::Compaction)))
        .unwrap();
    assert_eq!(r.model_id.as_str(), "opus");
    assert_eq!(r.pin_id.as_ref(), Some(&c.pin_id));
    // With the pin cooling, a title takes the requested pool's first eligible member ...
    fx.cool("opus", T0 + 1_000);
    let r = fx
        .read(|s| fx.router.route_auxiliary(s, &aux(Purpose::Title)))
        .unwrap();
    assert_eq!(r.model_id.as_str(), "haiku");
    assert_eq!(r.pin_id, None);
    // ... and the chat session's pin and every cursor are unchanged.
    let pin = fx.pin_row("s1").unwrap();
    assert_eq!((pin.model_id.as_str(), pin.revision), ("opus", 1));
    assert_eq!((fx.cursor("High"), fx.cursor("Low")), (1, 0));
    // A blocked auxiliary pool reports why, without writing anything.
    fx.dir.clear();
    let blocked = fx
        .read(|s| fx.router.route_auxiliary(s, &aux(Purpose::Enrichment)))
        .unwrap_err();
    assert_eq!(blocked.unavailable, Unavailable::Credentials);
    assert_eq!(blocked.codes, ["NoConnection"]);
}

#[test]
fn request_status_reads_the_operation_journal() {
    let fx = Fx::new();
    let router = fx.router.clone();
    let op = fx.write(move |tx| {
        router.admit_request(
            tx,
            &RequestAdmission {
                request_id: id("request-1"),
                purpose: Purpose::Conversation,
                session_id: Some(id("work-a")),
                project_id: Some(project("project-a")),
                root_id: None,
                model_id: id("opus"),
                connection_id: id("conn-claude"),
                pin_id: None,
                args_digest: "a".repeat(64),
            },
        )
    });
    let status = |ctx: &InvocationContext, r: &str| {
        fx.query_at(ctx, "router.request_status", json!({"requestId": r}), None)
    };
    match result(status(&fx.user(), "request-1")) {
        OperationResult::RouterRequestStatus { value } => {
            assert_eq!(value.operation_id, op);
            assert_eq!(value.state, OperationState::IntentRecorded);
        }
        other => panic!("{other:?}"),
    }
    let o = op.clone();
    fx.write(move |tx| {
        tx.operations()
            .begin_attempt(&o, 1, AttemptBinding::default())
    });
    match result(status(&fx.user(), "request-1")) {
        OperationResult::RouterRequestStatus { value } => {
            assert_eq!(value.state, OperationState::Preflighted)
        }
        other => panic!("{other:?}"),
    }
    // Unknown, or attributed to another project: not found (existence is not disclosed).
    assert_eq!(
        query_fault(status(&fx.user(), "request-9")).code,
        DomainErrorCode::NotFound
    );
    let other = fx.agent("agent-b", "project-b", "work-b");
    assert_eq!(
        query_fault(status(&other, "request-1")).code,
        DomainErrorCode::NotFound
    );
    // The binding is unique.
    let router = fx.router.clone();
    let err = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        router.admit_request(
            tx,
            &RequestAdmission {
                request_id: id("request-1"),
                purpose: Purpose::Title,
                session_id: None,
                project_id: None,
                root_id: None,
                model_id: id("opus"),
                connection_id: id("conn-claude"),
                pin_id: None,
                args_digest: "b".repeat(64),
            },
        )
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::Duplicate { .. }), "{err:?}");
}

#[test]
fn only_the_session_owner_routes_it_at_its_current_epoch() {
    let fx = Fx::new();
    // An agent routes its own session from an invocation bound to it.
    let mine = fx.agent("agent-a", "project-a", "work-a");
    assert_eq!(
        model_of(fx.route_as(&mine, "work-a", tier("Low"), "Default", &[])),
        "haiku"
    );
    // Another project's session is not visible; another session of its own is not bound.
    let other = fx.agent("agent-b", "project-b", "work-b");
    let fault = refused(fx.route_as(&other, "work-a", tier("Low"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    let unbound = fx.agent("agent-a", "project-a", "s1");
    let fault = refused(fx.route_as(&unbound, "work-a", tier("Low"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::PermissionDenied);
    // A stale context epoch is refused for refresh.
    let fault = refused(fx.command(
        &fx.user(),
        "router.route",
        json!({"sessionId": "s1", "expectedContextEpoch": "3", "intent": tier("Low"),
            "effort": "Default", "requiredCapabilities": []}),
    ));
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
    assert_eq!(fault.retry, RetryAdvice::RefreshAndNewRequest);
    // A closed session is not routed.
    fx.write(|tx| {
        let mut s = tx.sessions().session(&id("s2"))?.unwrap();
        s.closed_at_ms = Some(T0);
        s.closed_by = Some(actor("user-a"));
        s.revision += 1;
        tx.sessions().update_session(&s)
    });
    let fault = refused(fx.route("s2", tier("Low"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::Conflict);
    // No refusal moved a cursor beyond the one successful allocation.
    assert_eq!(fx.cursor("Low"), 1);
}

#[test]
fn availability_revision_2_reports_the_session_pin() {
    let fx = Fx::new();
    let (c, _) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    let a = fx.availability(tier("High"), "Default", &[], Some("s1"));
    assert_eq!(
        a.pin.as_ref().map(|p| p.pin_id.clone()),
        Some(c.pin_id.clone())
    );
    assert_eq!(a.pin_eligible, Some(true));
    assert_eq!(a.pin_reason.as_ref().map(|r| r.0.as_str()), Some("Initial"));
    // Asked about another selection, the pin is shown but not eligible for it.
    let b = fx.availability(tier("Low"), "Default", &[], Some("s1"));
    assert_eq!(b.pin_eligible, Some(false));
    fx.cool("opus", T0 + 1);
    assert_eq!(
        fx.availability(tier("High"), "Default", &[], Some("s1"))
            .pin_eligible,
        Some(false)
    );
    // Revision 1 has no session member and no pin members.
    let r1 = fx.query_at(
        &fx.user(),
        "router.availability",
        json!({"intent": tier("High"), "effort": "Default", "requiredCapabilities": [],
            "sessionId": "s1"}),
        None,
    );
    assert!(matches!(r1, DomainQueryReply::Rejected { .. }), "{r1:?}");
    match result(fx.query_at(
        &fx.user(),
        "router.availability",
        json!({"intent": tier("High"), "effort": "Default", "requiredCapabilities": []}),
        None,
    )) {
        OperationResult::RouterAvailability { value } => {
            let v = serde_json::to_value(value).unwrap();
            assert!(
                v.get("pin").is_none() && v.get("pinEligible").is_none(),
                "{v}"
            );
        }
        other => panic!("{other:?}"),
    }
    // A session of another project is not disclosed.
    let other = fx.agent("agent-b", "project-b", "work-b");
    let fault = query_fault(fx.query_at(
        &other,
        "router.availability",
        json!({"intent": tier("High"), "effort": "Default", "requiredCapabilities": [],
            "sessionId": "work-a"}),
        Some(2),
    ));
    assert_eq!(fault.code, DomainErrorCode::NotFound);
}

#[test]
fn without_connections_every_route_reports_no_connection() {
    let fx = Fx::new();
    fx.dir.clear();
    let fault = refused(fx.route("s1", tier("Max"), "Default", &[]));
    assert_eq!(fault.code, DomainErrorCode::CredentialsRequired);
    let a = fx.availability(tier("Max"), "Default", &[], None);
    assert_eq!(codes(&a), ["NoConnection"]);
    assert_eq!(a.retry_at_ms, None, "no invented wait");
    assert!(fx.pin_row("s1").is_none());
    assert_eq!(fx.cursor("Max"), 0);
    // An empty tier says so instead of crossing to another tier.
    fx.set_members("Max", &[]);
    let a = fx.availability(tier("Max"), "Default", &[], None);
    assert_eq!(codes(&a), ["EmptyPool"]);
    fx.dir
        .set(CLAUDE, "conn-claude", ConnectionState::Connected, None);
    assert_eq!(
        refused(fx.route("s1", tier("Max"), "Default", &[])).code,
        DomainErrorCode::UnsupportedCapability
    );
}

#[test]
fn migration_13_is_additive_and_seeds_the_catalog_once() {
    let m = MIGRATIONS
        .iter()
        .find(|m| m.version == 13)
        .expect("migration 13");
    assert_eq!(
        m.checksum(),
        MIGRATION_0013_SHA256,
        "an applied migration was edited"
    );
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 11));
    let h = Harness::new();
    let store: SqliteStore = h.store();
    let n: i64 = store
        .with_writer_connection(|c| {
            super::super::conn::db(
                c.query_row("SELECT count(*) FROM router_models", [], |r| r.get(0)),
            )
        })
        .unwrap();
    assert_eq!(n, 9);
    // Edits survive a reopen: the seed is not reapplied.
    run_write(&store, WriteOptions::default(), |tx| {
        let mut m = tx.router().route_model(&id("luna"))?.unwrap();
        m.display_name = "Luna (edited)".into();
        let r = m.revision;
        tx.router().put_route_model(&m, Some(r))
    })
    .unwrap();
    drop(store);
    let store = h.store();
    let luna = read_snapshot(&store, &ReadOptions::default(), |r| {
        r.router().route_model(&id("luna"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        (luna.display_name.as_str(), luna.revision),
        ("Luna (edited)", 2)
    );
    // A stale catalog revision is a conflict.
    let err = run_write(&store, WriteOptions::default(), move |tx| {
        tx.router().put_route_model(&luna, Some(1))
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::RevisionConflict(_)), "{err:?}");
}

#[test]
fn pins_and_cursors_are_compare_and_set_in_the_store() {
    let fx = Fx::new();
    chosen(fx.route("s1", tier("High"), "Default", &[]));
    let pin = fx.pin_row("s1").unwrap();
    // A writer holding a stale revision cannot replace the pin.
    let mut stale = pin.clone();
    stale.model_id = id("sol");
    let err = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.router().put_session_pin(&stale, Some(pin.revision + 5))
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::RevisionConflict(_)), "{err:?}");
    // A second first pin of the same session is a duplicate, never a competing pin.
    let again = fx.pin_row("s1").unwrap();
    let err = run_write(&*fx.store, WriteOptions::default(), move |tx| {
        tx.router().put_session_pin(&again, None)
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::Duplicate { .. }), "{err:?}");
    // A cursor that moved since it was read cannot take the same turn again.
    let err = run_write(&*fx.store, WriteOptions::default(), |tx| {
        tx.router().advance_tier_cursor("High", 0)
    })
    .unwrap_err();
    assert!(matches!(err, StorageError::RevisionConflict(_)), "{err:?}");
    assert_eq!(fx.cursor("High"), 1);
    assert_eq!(fx.pin_row("s1").unwrap().model_id.as_str(), "opus");
}

#[test]
fn a_reused_pin_records_the_latest_requests_requirements() {
    let fx = Fx::new();
    fx.add_model(custom("plain", CLAUDE, false, None));
    let (text, _) = chosen(fx.route("s1", tier("High"), "Default", &[]));
    assert_eq!(text.model_id.as_str(), "opus");
    // The same pinned route serves an image request: it is kept, and the pin now records that
    // the conversation needs vision, atomically with this request's receipt.
    let body = json!({"sessionId": "s1", "expectedContextEpoch": "0", "intent": tier("High"),
        "effort": "Default", "requiredCapabilities": ["vision"]});
    let (vision, wrote) =
        chosen(fx.command_keyed(&fx.user(), "router.route", "vision-1", body.clone()));
    assert!(wrote, "the requirements changed");
    assert_eq!(
        (vision.model_id.as_str(), &vision.pin_id),
        ("opus", &text.pin_id)
    );
    assert_eq!(vision.pin_revision.get(), 2);
    let pin = fx.pin_row("s1").unwrap();
    assert_eq!(pin.required_capabilities, ["vision"]);
    assert_eq!(pin.reason, "Initial", "the route was not re-chosen");
    assert_eq!(fx.cursor("High"), 1, "no round-robin turn");
    // A replay is the recorded decision; it writes nothing more.
    let (decision, replayed) =
        recorded(fx.command_keyed(&fx.user(), "router.route", "vision-1", body));
    assert!(replayed && matches!(decision, DomainDecision::Applied { .. }));
    assert_eq!(fx.pin_row("s1").unwrap().revision, 2);
    // So an explicit change cannot move the conversation to a model without vision.
    let fault = refused(fx.pin("s1", 2, named("plain"), "Default"));
    assert_eq!(fault.code, DomainErrorCode::UnsupportedCapability);
    assert_eq!(fx.pin_row("s1").unwrap().model_id.as_str(), "opus");
    // Unchanged requirements reuse the pin without a write.
    let (same, wrote) = chosen(fx.route("s1", tier("High"), "Default", &["vision"]));
    assert!(!wrote);
    assert_eq!(same.pin_revision.get(), 2);
}
