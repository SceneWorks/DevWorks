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
    /// The host-owned activation of this agent turn, not caller-supplied tool data.
    activation: Option<(Id, i64)>,
    capability_registration: Option<Id>,
    authority_sources: Vec<Id>,
    worktree: Option<BoundWorktree>,
    /// Bound only by the trusted host's dedicated human interaction path, never wire data.
    human_decision: Option<String>,
    human_project_purge: Option<String>,
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

    /// Dedicated native confirmation of one exact reviewed project purge. This
    /// private marker is never a wire argument, approval grant or agent capability.
    pub fn project_purge_context(
        &self,
        context: &InvocationContext,
        input: &crate::operations::projects::PurgeInput,
    ) -> Option<InvocationContext> {
        if context.store_id != self.store_id
            || context.principal_kind != PrincipalKind::Human
            || context
                .project_id
                .as_ref()
                .is_some_and(|project| *project != input.project_id)
        {
            return None;
        }
        let mut bound = context.clone();
        bound.human_project_purge = Some(project_purge_digest(context, input)?);
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
            activation: None,
            capability_registration: None,
            authority_sources: Vec::new(),
            worktree: None,
            human_decision: None,
            human_project_purge: None,
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

    pub fn project_purge_matches(&self, input: &crate::operations::projects::PurgeInput) -> bool {
        self.principal_kind == PrincipalKind::Human
            && self.human_project_purge.is_some()
            && self.human_project_purge == project_purge_digest(self, input)
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
        self.activation = None;
        self
    }
    /// Binds the exact activation epoch from the trusted launch path after lineage resolution.
    pub fn with_activation(mut self, id: Id, owner_epoch: i64) -> Self {
        self.activation = Some((id, owner_epoch));
        self
    }
    pub fn activation(&self) -> Option<(&Id, i64)> {
        self.activation.as_ref().map(|(id, epoch)| (id, *epoch))
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
    crate::fingerprint::canonical_digest(&value).ok()
}

fn project_purge_digest(
    context: &InvocationContext,
    input: &crate::operations::projects::PurgeInput,
) -> Option<String> {
    use crate::protocol::Check;
    input.check().ok()?;
    if !input.confirm {
        return None;
    }
    let worktree = context.worktree.as_ref().map(|tree| serde_json::json!({
        "id":tree.id,"repository":tree.repository_id,"work":tree.work_id,"revision":tree.ownership_revision
    }));
    let reviewed = context.reviewed_operation.as_ref().map(|reviewed| serde_json::json!({
        "approval":reviewed.approval_id,"approver":reviewed.approver,"operation":reviewed.operation,
        "schemaRevision":reviewed.schema_revision,"requestDigest":reviewed.request_digest,"ownerBinding":reviewed.owner_binding
    }));
    let value = serde_json::json!({
        "purpose":"projects.purge", "input":input,
        "actor":context.principal,"invocation":context.invocation_id,"ingress":context.ingress_namespace,
        "store":context.store_id,"restore":context.restore_epoch,"policy":context.policy_revision,
        "project":context.project_id,"root":context.root_id,"assignment":context.assignment_id,
        "session":context.session_id,"jobRun":context.job_run_id,"registration":context.capability_registration,
        "authoritySources":context.authority_sources,"worktree":worktree,"reviewed":reviewed
    });
    crate::fingerprint::canonical_digest(&value).ok()
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

    #[test]
    fn project_purge_proof_requires_native_human_confirmation_and_exact_content_and_context() {
        use crate::operations::projects::PurgeInput;
        let ingress = HostIngress {
            store_id: StoreId(id("store-1")),
        };
        let input: PurgeInput = serde_json::from_value(json!({"projectId":"project-a","manifestId":"manifest-a","expectedRevision":"1","expectedDigest":"a".repeat(64),"expectedProjectRevision":"2","confirm":true})).unwrap();
        let generic = context(&ingress, PrincipalKind::Human);
        assert!(!generic.project_purge_matches(&input));
        let bound = ingress.project_purge_context(&generic, &input).unwrap();
        assert!(bound.project_purge_matches(&input));
        for kind in [
            PrincipalKind::Agent,
            PrincipalKind::Host,
            PrincipalKind::Schedule,
            PrincipalKind::Package,
            PrincipalKind::McpClient,
        ] {
            assert!(
                ingress
                    .project_purge_context(&context(&ingress, kind), &input)
                    .is_none()
            );
        }
        let other = HostIngress {
            store_id: StoreId(id("store-2")),
        };
        assert!(other.project_purge_context(&generic, &input).is_none());
        let foreign_scope =
            generic
                .clone()
                .with_lineage(Some(ProjectId(id("project-b"))), None, None);
        assert!(
            ingress
                .project_purge_context(&foreign_scope, &input)
                .is_none()
        );
        for (field, value) in [
            ("projectId", json!("project-b")),
            ("manifestId", json!("manifest-b")),
            ("expectedRevision", json!("2")),
            ("expectedDigest", json!("b".repeat(64))),
            ("expectedProjectRevision", json!("3")),
            ("confirm", json!(false)),
        ] {
            let mut changed = serde_json::to_value(&input).unwrap();
            changed[field] = value;
            let changed: PurgeInput = serde_json::from_value(changed).unwrap();
            assert!(!bound.project_purge_matches(&changed), "{field}");
            if !changed.confirm {
                assert!(ingress.project_purge_context(&generic, &changed).is_none());
            }
        }
        let changed = bound.clone().with_lineage(
            Some(ProjectId(id("project-a"))),
            Some(RootId(id("new-root"))),
            None,
        );
        assert!(!changed.project_purge_matches(&input));
        for field in [
            "store",
            "restore",
            "policy",
            "actor",
            "invocation",
            "ingress",
            "session",
            "authority",
        ] {
            let mut changed = bound.clone();
            match field {
                "store" => changed.store_id = StoreId(id("store-2")),
                "restore" => changed.restore_epoch = Counter::new(1).unwrap(),
                "policy" => changed.policy_revision = Revision::new(2).unwrap(),
                "actor" => changed.principal = ActorId(id("another-human")),
                "invocation" => changed.invocation_id = id("another-invocation"),
                "ingress" => changed.ingress_namespace = id("another-ingress"),
                "session" => changed.session_id = Some(id("another-session")),
                "authority" => changed.authority_sources.push(id("another-grant")),
                _ => unreachable!(),
            }
            assert!(!changed.project_purge_matches(&input), "{field}");
        }
    }
}
