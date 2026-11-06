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

/// A worktree identity resolved by trusted ingress from the durable ownership ledger.
/// Tool arguments cannot construct this value or replace its ownership revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundWorktree {
    id: Id,
    repository_id: Id,
    work_id: Id,
    ownership_revision: i64,
}
impl BoundWorktree {
    pub(crate) fn resolved(
        id: Id,
        repository_id: Id,
        work_id: Id,
        ownership_revision: i64,
    ) -> Self {
        Self {
            id,
            repository_id,
            work_id,
            ownership_revision,
        }
    }

    pub fn id(&self) -> &Id {
        &self.id
    }
    pub fn repository_id(&self) -> &Id {
        &self.repository_id
    }
    pub fn work_id(&self) -> &Id {
        &self.work_id
    }
    pub fn ownership_revision(&self) -> i64 {
        self.ownership_revision
    }
}

/// Read-only exact human-reviewed owner evidence. Only core restoration mints this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewedOperation {
    pub(crate) approval_id: Id,
    pub(crate) approver: ActorId,
    pub(crate) operation: String,
    pub(crate) schema_revision: u32,
    pub(crate) request_digest: String,
    pub(crate) owner_binding: serde_json::Value,
}
impl ReviewedOperation {
    pub fn approval_id(&self) -> &Id {
        &self.approval_id
    }
    pub fn approver(&self) -> &ActorId {
        &self.approver
    }
    pub fn operation(&self) -> &str {
        &self.operation
    }
    pub fn schema_revision(&self) -> u32 {
        self.schema_revision
    }
    pub fn request_digest(&self) -> &str {
        &self.request_digest
    }
    pub fn owner_binding(&self) -> &serde_json::Value {
        &self.owner_binding
    }
}

/// Host-bound invocation context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationContext {
    principal: ActorId,
    principal_kind: PrincipalKind,
    invocation_id: Id,
    session_id: Option<Id>,
    /// Actual durable job occurrence, bound only by the owning core job runner.
    job_run_id: Option<Id>,
    ingress_namespace: Id,
    store_id: StoreId,
    restore_epoch: Counter,
    policy_revision: Revision,
    project_id: Option<ProjectId>,
    root_id: Option<RootId>,
    assignment_id: Option<AssignmentId>,
    capability_registration: Option<Id>,
    authority_sources: Vec<Id>,
    worktree: Option<BoundWorktree>,
    /// Bound only by the trusted host's dedicated human interaction path, never wire data.
    human_decision: Option<String>,
    reviewed_operation: Option<ReviewedOperation>,
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
    /// Binds the exact decision from a trusted native human interaction. Generic commands,
    /// tools and model messages cannot add this marker to an invocation or serialize it.
    /// The decision service also verifies the principal's registered human identity.
    pub fn human_decision_context(
        &self,
        context: &InvocationContext,
        decision: &crate::operations::approvals::RecordHumanDecisionInput,
    ) -> Option<InvocationContext> {
        if context.store_id != self.store_id || context.principal_kind != PrincipalKind::Human {
            return None;
        }
        let mut bound = context.clone();
        bound.human_decision = Some(decision_digest(decision)?);
        Some(bound)
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
            job_run_id: None,
            ingress_namespace,
            store_id,
            restore_epoch,
            policy_revision,
            project_id: None,
            root_id: None,
            assignment_id: None,
            capability_registration: None,
            authority_sources: Vec::new(),
            worktree: None,
            human_decision: None,
            reviewed_operation: None,
        }
    }

    /// Whether trusted human ingress bound this exact, complete decision. This is not a grant
    /// and is never carried in an approval proposal, a tool argument or a response.
    pub fn human_decision_matches(
        &self,
        decision: &crate::operations::approvals::RecordHumanDecisionInput,
    ) -> bool {
        self.principal_kind == PrincipalKind::Human
            && self.human_decision.is_some()
            && self.human_decision == decision_digest(decision)
    }

    pub fn reviewed_operation(&self) -> Option<&ReviewedOperation> {
        self.reviewed_operation.as_ref()
    }
    pub(crate) fn with_reviewed_operation(mut self, reviewed: ReviewedOperation) -> Self {
        self.reviewed_operation = Some(reviewed);
        self
    }

    pub(crate) fn with_job_run(mut self, run_id: Id) -> Self {
        self.job_run_id = Some(run_id);
        self
    }
    /// Original scheduled occurrence; this is provenance, never permission or a dispatch grant.
    pub fn job_run_id(&self) -> Option<&Id> {
        self.job_run_id.as_ref()
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
        self.worktree = None;
        self.reviewed_operation = None;
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

    pub(crate) fn bind_worktree(mut self, binding: BoundWorktree) -> Self {
        self.worktree = Some(binding);
        self
    }
    pub fn worktree(&self) -> Option<&BoundWorktree> {
        self.worktree.as_ref()
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

fn decision_digest(
    decision: &crate::operations::approvals::RecordHumanDecisionInput,
) -> Option<String> {
    let value = serde_json::to_value(decision).ok()?;
    let canonical = crate::jcs::canonicalize(&value).ok()?;
    Some(crate::fingerprint::sha256_hex(canonical.as_bytes()))
}

/// Host encoding of store identity, restore epoch and ingress namespace. Opaque IDs cannot
/// contain `/`, so the encoding is injective. A caller cannot choose it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScopeKey(String);
impl ScopeKey {
    pub fn of(ctx: &InvocationContext) -> Self {
        let base = format!(
            "{}/{}/{}",
            ctx.store_id.0.as_str(),
            ctx.restore_epoch.as_str(),
            ctx.ingress_namespace.as_str()
        );
        // Preserve every unbound receipt namespace. A bound retry selects the immutable
        // worktree identity, not its changing head or ownership epoch.
        Self(match ctx.worktree() {
            Some(tree) => format!("{base}/worktree/{}", tree.id().as_str()),
            None => base,
        })
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
    /// The identity of a receipt the gateway recorded, from its stored parts (the receipt reply
    /// publisher reads them from the intent the gateway committed with the receipt). Never from
    /// a request.
    pub(crate) fn recorded(actor_id: ActorId, scope_key: String, request_key: RequestKey) -> Self {
        Self {
            actor_id,
            scope_key: ScopeKey(scope_key),
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

#[cfg(test)]
mod human_decision_tests {
    use super::*;
    use crate::operations::approvals::RecordHumanDecisionInput;
    use serde_json::json;

    fn id(value: &str) -> Id {
        Id::new(value).unwrap()
    }
    fn decision() -> RecordHumanDecisionInput {
        serde_json::from_value(json!({"proposalId":"proposal-1", "expectedRevision":"1",
            "contentDigest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "decision":"Approve", "rationale":"Approve this exact change"}))
        .unwrap()
    }
    fn context(ingress: &HostIngress, kind: PrincipalKind) -> InvocationContext {
        ingress.context(
            ActorId(id("user-1")),
            kind,
            id("invocation-1"),
            id("native-ui"),
            Counter::new(0).unwrap(),
            Revision::new(1).unwrap(),
        )
    }

    #[test]
    fn only_human_ingress_binds_the_exact_complete_decision() {
        let ingress = HostIngress {
            store_id: StoreId(id("store-1")),
        };
        let original = decision();
        let generic = context(&ingress, PrincipalKind::Human);
        assert!(!generic.human_decision_matches(&original));
        let bound = ingress.human_decision_context(&generic, &original).unwrap();
        assert!(bound.human_decision_matches(&original));
        for kind in [
            PrincipalKind::Agent,
            PrincipalKind::Host,
            PrincipalKind::Schedule,
            PrincipalKind::Package,
            PrincipalKind::McpClient,
        ] {
            assert!(
                ingress
                    .human_decision_context(&context(&ingress, kind), &original)
                    .is_none()
            );
        }
        let other = HostIngress {
            store_id: StoreId(id("store-2")),
        };
        assert!(other.human_decision_context(&generic, &original).is_none());
        for (field, value) in [
            ("proposalId", json!("proposal-2")),
            ("expectedRevision", json!("2")),
            (
                "contentDigest",
                json!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ),
            ("decision", json!("Reject")),
            ("rationale", json!("Different rationale")),
        ] {
            let mut changed = serde_json::to_value(&original).unwrap();
            changed[field] = value;
            let changed: RecordHumanDecisionInput = serde_json::from_value(changed).unwrap();
            assert!(!bound.human_decision_matches(&changed));
        }
    }
}
