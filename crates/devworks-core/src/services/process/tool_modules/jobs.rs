//! The recorded payloads of execute and watch operations.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExecuteJob {
    pub(super) actor: crate::protocol::ActorId,
    pub(super) kind: String,
    pub(super) session: Option<Id>,
    pub(super) project: Option<ProjectId>,
    pub(super) root: Option<crate::protocol::RootId>,
    pub(super) assignment: Option<crate::protocol::AssignmentId>,
    pub(super) restore_epoch: i64,
    pub(super) policy_revision: i64,
    pub(super) fingerprint: String,
    #[serde(default)]
    pub(super) authority_sources: Vec<Id>,
    pub(super) spec: ProcessSpec,
    /// A watcher's output tail budget; zero (omitted) for a plain `process.execute`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) tail_bytes: u32,
    /// Set when `watch.create` admitted this launch: the effect re-authorizes that request (its
    /// grant binds the watch.create fingerprint), not a `process.execute` request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) watch: Option<WatchOrigin>,
}

pub(super) fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct WatchOrigin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) requested_tail_bytes: Option<u32>,
    /// A watcher's regular expression, counted over the output as it streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pattern: Option<String>,
}

/// Who admitted a process launch, which decides its consumer and its re-authorization.
#[derive(Clone, Debug)]
pub(crate) enum ExecuteOrigin {
    /// `process.execute`.
    Tool,
    /// `watch.create`, with its effective and requested tail budgets.
    Watch {
        tail_bytes: u32,
        requested: Option<u32>,
        /// The watcher's regular expression over the output, if it matches one.
        pattern: Option<String>,
    },
}

impl ExecuteJob {
    pub(super) fn from_context(
        ctx: &InvocationContext,
        fingerprint: &Fingerprint,
        spec: ProcessSpec,
    ) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            fingerprint: fingerprint.as_str().to_owned(),
            authority_sources: ctx.authority_sources().to_vec(),
            spec,
            tail_bytes: 0,
            watch: None,
        }
    }

    pub(super) fn context(
        &self,
        store: &dyn ApplicationStore,
        operation: &OperationId,
    ) -> Option<InvocationContext> {
        process_context(
            store,
            operation,
            &self.actor,
            &self.kind,
            self.session.as_ref(),
            self.project.as_ref(),
            self.root.as_ref(),
            self.assignment.as_ref(),
            self.restore_epoch,
            self.policy_revision,
            &self.authority_sources,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn process_context(
    store: &dyn ApplicationStore,
    operation: &OperationId,
    actor: &crate::protocol::ActorId,
    kind: &str,
    session: Option<&Id>,
    project: Option<&ProjectId>,
    root: Option<&crate::protocol::RootId>,
    assignment: Option<&crate::protocol::AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    authority_sources: &[Id],
) -> Option<InvocationContext> {
    let kind = PrincipalKind::parse(kind)?;
    let mut context = InvocationContext::new(
        actor.clone(),
        kind,
        operation.0.clone(),
        Id::new(EFFECT_CONSUMER).expect("process effect namespace"),
        store.info().store_id,
        counter(restore_epoch),
        rev(policy_revision),
    )
    .with_lineage(project.cloned(), root.cloned(), assignment.cloned());
    if let Some(session) = session {
        context = context.with_session(session.clone());
    }
    for source in authority_sources {
        context = context.with_authority_source(source.clone());
    }
    Some(context)
}
