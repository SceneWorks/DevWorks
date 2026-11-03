//! Host-created invocation identity and receipt identity.
//!
//! [`InvocationContext`] is the only source of actor, scope, lineage and authority references
//! for an admitted request. It deliberately implements neither `Deserialize` nor `Default`, so no
//! tool argument, model message, package frame or JSON body can produce one; adapters (UI, tool,
//! schedule, package host, MCP) construct it from records the host has already resolved.
//!
//! ```compile_fail
//! // A context can never be decoded from untrusted JSON.
//! let _: devworks_core::context::InvocationContext = serde_json::from_str("{}").unwrap();
//! ```
use crate::protocol::{
    ActorId, AssignmentId, Counter, Id, ProjectId, RequestKey, Revision, RootId, StoreId,
};

/// Who is acting. An agent stays attributed to itself even when a human authorization source
/// is linked; it never becomes the human actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    Human,
    Agent,
    Package,
    /// An authorized incoming MCP client identity (MCP server surface).
    McpClient,
    Schedule,
    Host,
}

/// Host-bound invocation context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationContext {
    principal: ActorId,
    principal_kind: PrincipalKind,
    invocation_id: Id,
    session_id: Option<Id>,
    ingress_namespace: Id,
    store_id: StoreId,
    restore_epoch: Counter,
    policy_revision: Revision,
    project_id: Option<ProjectId>,
    root_id: Option<RootId>,
    assignment_id: Option<AssignmentId>,
    capability_registration: Option<Id>,
    authority_sources: Vec<Id>,
}
/// The host's trusted ingress for one store: the only public way to create an
/// [`InvocationContext`]. It is created from the store itself, so only code the composition
/// root gave the store (ingress adapters: UI, tool host, schedule runner, package host, MCP
/// server) can mint contexts; a use case receives a context, a command and a transaction, none
/// of which yields one.
///
/// ```compile_fail
/// // A context cannot be built without the trusted ingress.
/// use devworks_core::context::*;
/// let _ = InvocationContext::new(todo!(), PrincipalKind::Human, todo!(), todo!(), todo!(), todo!(), todo!());
/// ```
#[derive(Clone, Debug)]
pub struct HostIngress {
    store_id: StoreId,
}
impl HostIngress {
    pub fn new(store: &dyn crate::ports::storage::ApplicationStore) -> Self {
        Self {
            store_id: store.info().store_id,
        }
    }
    /// Binds an invocation the host has authenticated (see [`InvocationContext`]).
    pub fn context(
        &self,
        principal: ActorId,
        principal_kind: PrincipalKind,
        invocation_id: Id,
        ingress_namespace: Id,
        restore_epoch: Counter,
        policy_revision: Revision,
    ) -> InvocationContext {
        InvocationContext::new(
            principal,
            principal_kind,
            invocation_id,
            ingress_namespace,
            self.store_id.clone(),
            restore_epoch,
            policy_revision,
        )
    }
}

impl InvocationContext {
    /// Binds an invocation the host has authenticated. `ingress_namespace` separates retry-key
    /// namespaces (a UI surface, one durable tool-call lineage, a schedule): different sessions
    /// do not share one automatically. Outside core, contexts come from [`HostIngress`].
    pub(crate) fn new(
        principal: ActorId,
        principal_kind: PrincipalKind,
        invocation_id: Id,
        ingress_namespace: Id,
        store_id: StoreId,
        restore_epoch: Counter,
        policy_revision: Revision,
    ) -> Self {
        Self {
            principal,
            principal_kind,
            invocation_id,
            session_id: None,
            ingress_namespace,
            store_id,
            restore_epoch,
            policy_revision,
            project_id: None,
            root_id: None,
            assignment_id: None,
            capability_registration: None,
            authority_sources: Vec::new(),
        }
    }
    pub fn with_session(mut self, session_id: Id) -> Self {
        self.session_id = Some(session_id);
        self
    }
    /// Host-resolved project/work/assignment lineage of the invoking agent or surface.
    pub fn with_lineage(
        mut self,
        project_id: Option<ProjectId>,
        root_id: Option<RootId>,
        assignment_id: Option<AssignmentId>,
    ) -> Self {
        self.project_id = project_id;
        self.root_id = root_id;
        self.assignment_id = assignment_id;
        self
    }
    /// Current capability registration (tool/package contribution) the call was made through.
    pub fn with_capability_registration(mut self, registration: Id) -> Self {
        self.capability_registration = Some(registration);
        self
    }
    /// Links a host authority-source record (for example a user-minted authorization receipt).
    /// The authority port still resolves and revalidates it; the reference alone grants nothing.
    pub fn with_authority_source(mut self, source: Id) -> Self {
        self.authority_sources.push(source);
        self
    }

    pub fn principal(&self) -> &ActorId {
        &self.principal
    }
    pub fn principal_kind(&self) -> PrincipalKind {
        self.principal_kind
    }
    pub fn invocation_id(&self) -> &Id {
        &self.invocation_id
    }
    pub fn session_id(&self) -> Option<&Id> {
        self.session_id.as_ref()
    }
    pub fn ingress_namespace(&self) -> &Id {
        &self.ingress_namespace
    }
    pub fn store_id(&self) -> &StoreId {
        &self.store_id
    }
    pub fn restore_epoch(&self) -> &Counter {
        &self.restore_epoch
    }
    pub fn policy_revision(&self) -> &Revision {
        &self.policy_revision
    }
    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }
    pub fn root_id(&self) -> Option<&RootId> {
        self.root_id.as_ref()
    }
    pub fn assignment_id(&self) -> Option<&AssignmentId> {
        self.assignment_id.as_ref()
    }
    pub fn capability_registration(&self) -> Option<&Id> {
        self.capability_registration.as_ref()
    }
    pub fn authority_sources(&self) -> &[Id] {
        &self.authority_sources
    }
}

/// Host encoding of store identity, restore epoch and ingress namespace. Opaque IDs cannot
/// contain `/`, so the encoding is injective. A caller cannot choose it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeKey(String);
impl ScopeKey {
    pub fn of(ctx: &InvocationContext) -> Self {
        Self(format!(
            "{}/{}/{}",
            ctx.store_id.0.as_str(),
            ctx.restore_epoch.as_str(),
            ctx.ingress_namespace.as_str()
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Receipt identity `(actorId, scopeKey, requestKey)`. Formed only by the gateway from a host
/// context; the request key is a retry identity within that namespace, not authority.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReceiptIdentity {
    actor_id: ActorId,
    scope_key: ScopeKey,
    request_key: RequestKey,
}
impl ReceiptIdentity {
    pub(crate) fn bind(ctx: &InvocationContext, request_key: RequestKey) -> Self {
        Self {
            actor_id: ctx.principal.clone(),
            scope_key: ScopeKey::of(ctx),
            request_key,
        }
    }
    pub fn actor_id(&self) -> &ActorId {
        &self.actor_id
    }
    pub fn scope_key(&self) -> &ScopeKey {
        &self.scope_key
    }
    pub fn request_key(&self) -> &RequestKey {
        &self.request_key
    }
}
