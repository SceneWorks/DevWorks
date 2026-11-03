//! `ConversationService`: sessions, transcripts, briefings and context sources behind
//! `sessions.list`, `.get`, `.transcript`, `.briefing`, `.append_message`, `.context_sources`,
//! `.start` and `.end`, registered through [`crate::services::UseCases::with_sessions`]. The
//! native UI and agent tools reach the same use cases through the gateway; the host runtime
//! appends assistant turns, tool calls/results, notices, epochs and checkpoints through the same
//! session writer ([`transcript::SessionWriter`], [`transcript::append_session_batch`]).
//!
//! # Scope (HISTORY-01, access)
//!
//! Current authority (role, selected tools, reviewer read-only, project confinement) is the
//! gateway's authority port. On top of it every use case checks the session's scope: an
//! invocation bound to a project sees only that project's sessions; a workspace invocation sees
//! every session. Anything else is `NotFound`, so existence is not disclosed. A work session
//! belongs to its root's project.
//!
//! # Lifecycle
//!
//! `sessions.start` creates the conversation and session (revision 1, epoch 0) with its work
//! lineage and a `session` link on the root; a retried start replays its receipt, so there is
//! never a duplicate Home session. `sessions.end` closes it under its expected revision (ending
//! a closed session is `NoChange`); later appends are refused, while the transcript, its calls
//! and checkpoints stay readable. `sessions.append_message` appends exactly one user message
//! (text and attachments that resolve to visible artifact versions) at the session's next
//! sequence and current epoch; a public message cannot write a host notice, a tool result, an
//! actor or an epoch. Only the user (a human principal) or the session's own agent or creator
//! may message or end a session, and an agent only from an invocation bound to that session.
//! A human's Home session is bound to the configured workspace coordinator agent, so the
//! runtime writes its turns. Workspace scope lists Home sessions only; Home and the sidebar
//! resolve the same open Home session by that scope.
//!
//! # Native material
//!
//! Provider-native continuation items are restricted transport material: `sessions.transcript`
//! returns their references only to the session's creator, its agent or the host, omits them for
//! everyone else, and records an attributed `NativeMaterialViewed` event (its own unit of work)
//! before serving them; a view that cannot be recorded is not served. The transcript page always
//! carries a continuation, including at its tail, so a view polls for new entries (the live
//! wake-up hint is the shell's event channel, not this query).
//!
//! # Retained CodeGraph behavior
//!
//! `WorkController.StartSession`/`EndSession`/`Briefing` and the `start_session`, `end_session`
//! and `get_briefing` work tools map to `sessions.start`, `.end` and `.briefing`; `AskController`
//! chats and run transcripts and `WorkViewsController.Sessions` map to `sessions.list`,
//! `.transcript` and `.append_message`. Run control (`AskController.CancelRun`, `runs.stop`)
//! belongs to the run-control item; this service provides the transcript and session data it
//! reads.
pub mod transcript;
pub mod turn;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, sessions as ops};
use crate::ports::storage::{
    ApplicationStore, NewEvent, ReadSnapshot, SessionPosition, SessionScope, StorageError,
    StoredLink, StoredSession, WorkRoot, WriteOptions, WriteTransaction, run_write,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::{
    ArtifactRef, SessionCommandRequest, SessionQueryRequest, TranscriptBody, TranscriptEntry,
    TranscriptPage,
};
use crate::protocol::records::{ContextSource, SessionRecord};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    ActorId, CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, ProjectId,
    Protocol, RetryAdvice, Revision, RevisionDimension, RootId,
};
use crate::services::cursor::CursorBinding;
use serde_json::json;
use std::sync::Arc;
use transcript::{AppendPath, BatchOutcome, BatchRefusal, SESSION_AGGREGATE, SessionBatch};

/// The operations [`SessionService`] is registered for.
pub const OPERATIONS: [&str; 8] = [
    "sessions.list",
    "sessions.get",
    "sessions.transcript",
    "sessions.briefing",
    "sessions.append_message",
    "sessions.context_sources",
    "sessions.start",
    "sessions.end",
];
/// Most context sources a briefing composes; the rest page through `sessions.context_sources`.
pub const BRIEFING_LIMIT: usize = 32;
/// Most pending calls and waiting operations a briefing names.
pub const BRIEFING_OPEN_ITEMS: u16 = 8;
/// Fault target kind of an append to an ended session.
pub const SESSION_CLOSED: &str = "SessionClosed";
/// Fault target kind of a briefing or source listing for a superseded context epoch.
pub const EPOCH_CHANGED: &str = "context_epoch";

// ---------------------------------------------------------------------------------------------
// Refusals and helpers shared with the history service

/// A typed refusal: a durable `Refused` decision for commands, a fault for queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: DomainErrorCode,
    pub retry: RetryAdvice,
    pub target_kind: &'static str,
    pub target_id: Option<Id>,
}
impl Refusal {
    pub fn new(code: DomainErrorCode, target_kind: &'static str, target_id: Option<Id>) -> Self {
        Self {
            code,
            retry: RetryAdvice::Never,
            target_kind,
            target_id,
        }
    }
    pub fn retry(mut self, retry: RetryAdvice) -> Self {
        self.retry = retry;
        self
    }
    pub fn fault(self, correlation_id: CorrelationId) -> DomainFault {
        let mut f = crate::services::fault(self.code, self.retry, &correlation_id);
        f.target_id = self.target_id;
        f.target_kind = Some(BoundedText(self.target_kind.to_owned()));
        f
    }
}
pub(crate) fn not_found(kind: &'static str, id: &Id) -> Refusal {
    Refusal::new(DomainErrorCode::NotFound, kind, Some(id.clone()))
}

/// A use-case failure: a typed refusal or a storage error.
#[derive(Debug)]
pub enum Failure {
    Refused(Refusal),
    Storage(StorageError),
}
impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}

/// The query fault of a failure (cursor expiry, cancellation and storage map to their codes).
pub(crate) fn query_fault(
    f: Failure,
    kind: &'static str,
    correlation: CorrelationId,
) -> DomainFault {
    match f {
        Failure::Refused(r) => r.fault(correlation),
        Failure::Storage(e) => {
            let (code, retry) = match e {
                StorageError::CursorExpired => (
                    DomainErrorCode::CursorExpired,
                    RetryAdvice::RefreshAndNewRequest,
                ),
                StorageError::Cancelled => (DomainErrorCode::Cancelled, RetryAdvice::Never),
                StorageError::IncompatibleSchema(_) => {
                    (DomainErrorCode::SchemaMismatch, RetryAdvice::Never)
                }
                _ => (
                    DomainErrorCode::StorageUnavailable,
                    RetryAdvice::SameCommand,
                ),
            };
            Refusal::new(code, kind, None)
                .retry(retry)
                .fault(correlation)
        }
    }
}

/// The command decision of a use-case outcome.
pub(crate) fn decided(
    tx: &mut dyn WriteTransaction,
    outcome: Result<DomainDecision, Failure>,
) -> Result<Decided, HandlerFailure> {
    match outcome {
        Ok(decision) => Ok(Decided {
            decision,
            operation_id: None,
        }),
        Err(Failure::Refused(r)) => Ok(Decided {
            decision: DomainDecision::Refused {
                fault: r.fault(CorrelationId(tx.new_id(IdKind::Correlation))),
            },
            operation_id: None,
        }),
        Err(Failure::Storage(e)) => Err(HandlerFailure::Storage(e)),
    }
}

pub(crate) fn rev(n: i64) -> Revision {
    Revision::new(n.max(1)).expect("positive revision")
}
pub(crate) fn counter(n: i64) -> Counter {
    Counter::new(n.max(0)).expect("non-negative counter")
}
pub(crate) fn label(s: &str, fallback: &str) -> BoundedText<1, 256> {
    let t: String = s.chars().take(256).collect();
    BoundedText(if t.is_empty() { fallback.into() } else { t })
}
pub(crate) fn content(s: &str) -> BoundedText<0, 65536> {
    BoundedText(s.chars().take(65536).collect())
}
/// Tokens of `bytes` by the conservative attributed estimate (4 bytes per token, rounded up).
pub(crate) fn token_estimate(bytes: i64) -> i64 {
    (bytes.max(0) + 3) / 4
}

/// A request naming `project` must match the invocation's project, when it has one.
pub(crate) fn require_project(ctx: &InvocationContext, project: &ProjectId) -> Result<(), Refusal> {
    match ctx.project_id() {
        Some(p) if p != project => Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "project",
            Some(project.0.clone()),
        )),
        _ => Ok(()),
    }
}
/// Whether the invocation may see a session of `project` (`None`: a workspace session).
pub(crate) fn project_visible(ctx: &InvocationContext, project: Option<&ProjectId>) -> bool {
    match ctx.project_id() {
        None => true,
        Some(p) => project == Some(p),
    }
}
/// The work root `id` if this invocation may see it (otherwise `NotFound`).
pub(crate) fn visible_root(
    snapshot: &dyn crate::ports::storage::SessionRead,
    ctx: &InvocationContext,
    id: &RootId,
) -> Result<WorkRoot, Failure> {
    match snapshot.work_root(id)? {
        Some(w) if project_visible(ctx, Some(&w.project_id)) => Ok(w),
        _ => Err(not_found("root", &id.0).into()),
    }
}
fn visible_session(
    read: &dyn crate::ports::storage::SessionRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredSession, Failure> {
    match read.session(id)? {
        Some(s) if project_visible(ctx, s.project_id.as_ref()) => Ok(s),
        _ => Err(not_found("session", id).into()),
    }
}

/// The wire record of a session.
pub fn session_record(s: &StoredSession) -> SessionRecord {
    SessionRecord {
        id: s.id.clone(),
        revision: rev(s.revision),
        title: label(&s.title, "session"),
        root_id: s.root_id.clone(),
        context_epoch: counter(s.context_epoch),
        closed: s.closed(),
    }
}

fn session_event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    s: &StoredSession,
    kind: &str,
    payload: serde_json::Value,
) -> Result<(EventId, i64), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    let mut payload = payload;
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
        map.insert("invocationId".into(), json!(ctx.invocation_id().as_str()));
    }
    let seq = tx.history().append(NewEvent {
        id: id.clone(),
        actor: ctx.principal().clone(),
        project_id: s.project_id.clone(),
        root_id: s.root_id.clone(),
        operation_id: None,
        aggregate_kind: SESSION_AGGREGATE.into(),
        aggregate_id: s.id.as_str().to_owned(),
        aggregate_revision: s.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok((id, seq))
}

/// Appends a lineage link of a session fact on its work root (no root: no link).
fn session_link(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    s: &StoredSession,
    kind: &str,
    parent: Option<Id>,
    event: (EventId, i64),
    summary: String,
) -> Result<(), StorageError> {
    let (Some(root), Some(project)) = (&s.root_id, &s.project_id) else {
        return Ok(());
    };
    let link = StoredLink {
        id: tx.new_id(IdKind::Link),
        project_id: project.clone(),
        root_id: root.clone(),
        kind: kind.into(),
        target_kind: "session".into(),
        target_id: s.id.as_str().to_owned(),
        parent_id: parent,
        supersedes_id: None,
        actor: ctx.principal().clone(),
        event_id: event.0.0,
        sequence: event.1,
        repository_id: None,
        revision: None,
        summary,
        body: json!({"sessionId": s.id.as_str(), "revision": s.revision}),
        created_at_ms: tx.now_ms(),
    };
    tx.sessions().insert_link(&link)
}

/// The fault of a refused transcript append.
pub fn append_refusal(r: &BatchRefusal) -> Refusal {
    use DomainErrorCode::*;
    match r {
        BatchRefusal::SessionNotFound => Refusal::new(NotFound, "session", None),
        BatchRefusal::SessionClosed => Refusal::new(Conflict, SESSION_CLOSED, None),
        BatchRefusal::UnknownReference(what) => Refusal::new(NotFound, what, None),
        BatchRefusal::UnknownOperation(op) => {
            Refusal::new(NotFound, "operation", Some(op.0.clone()))
        }
        BatchRefusal::StaleSequence { .. } | BatchRefusal::SequenceGap { .. } => {
            Refusal::new(Conflict, "transcript_sequence", None)
                .retry(RetryAdvice::RefreshAndNewRequest)
        }
        BatchRefusal::Unauthorized(_) => Refusal::new(PermissionDenied, "session_writer", None),
        // The same batch identity with another payload: the contract's conflict, like a reused
        // request key with a different intent.
        BatchRefusal::PayloadChanged => Refusal::new(Conflict, "transcript_batch", None)
            .retry(RetryAdvice::RefreshAndNewRequest),
        BatchRefusal::UnresolvedToolCalls => Refusal::new(Conflict, "unresolved_tool_calls", None)
            .retry(RetryAdvice::AfterBlockerClears),
        _ => Refusal::new(InvalidArgument, "transcript_batch", None),
    }
}

// ---------------------------------------------------------------------------------------------
// The service

pub struct SessionService {
    ids: Arc<dyn IdSource>,
    /// Records the view audit of served native material (its own small unit of work).
    audit: Arc<dyn ApplicationStore>,
    /// The workspace coordinator agent a human's Home session is bound to, when configured.
    coordinator: Option<ActorId>,
}

/// History kind of a transcript read that served restricted native material.
pub const NATIVE_VIEWED: &str = "NativeMaterialViewed";
/// Fault target kind of a session command from a principal that may not act on the session.
pub const SESSION_PRINCIPAL: &str = "session_principal";

impl SessionService {
    pub fn new(ids: Arc<dyn IdSource>, audit: Arc<dyn ApplicationStore>) -> Self {
        Self {
            ids,
            audit,
            coordinator: None,
        }
    }
    /// Binds every Home (workspace, no work root) session a human starts to `coordinator`, the
    /// agent that conducts it, so the host runtime may append its turns.
    pub fn with_coordinator(mut self, coordinator: ActorId) -> Self {
        self.coordinator = Some(coordinator);
        self
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Who may message or end a session: the user (a human principal, by human ingress), or its
    /// own agent or creator — and an agent only from an invocation bound to that session. A
    /// public message never changes its actor or role; it is attributed to its principal.
    fn may_act(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        session: &StoredSession,
    ) -> Result<(), Failure> {
        let denied = || {
            Failure::from(Refusal::new(
                DomainErrorCode::PermissionDenied,
                SESSION_PRINCIPAL,
                Some(session.id.clone()),
            ))
        };
        let principal = ctx.principal();
        let registered = tx.actors().actor_kind(principal)?;
        if registered != Some(ctx.principal_kind()) {
            return Err(denied());
        }
        if ctx.principal_kind() == PrincipalKind::Human {
            return Ok(());
        }
        if principal != &session.agent_id && principal != &session.created_by {
            return Err(denied());
        }
        if ctx.principal_kind() == PrincipalKind::Agent && ctx.session_id() != Some(&session.id) {
            return Err(denied());
        }
        Ok(())
    }

    /// Records one attributed `NativeMaterialViewed` event for a transcript page that serves
    /// native items, in its own unit of work (a read never writes through its snapshot).
    fn audit_native_view(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        session: &StoredSession,
        sequences: Vec<i64>,
    ) -> Result<(), Failure> {
        let (ctx, s) = (ctx.clone(), session.clone());
        let query_id = query.query_id().0.as_str().to_owned();
        run_write(&*self.audit, WriteOptions::default(), move |tx| {
            if &tx.store().store_id != ctx.store_id() {
                return Err(StorageError::ScopeDenied);
            }
            session_event(
                tx,
                &ctx,
                &s,
                NATIVE_VIEWED,
                json!({"sequences": sequences, "queryId": query_id}),
            )
            .map(|_| ())
        })?;
        Ok(())
    }

    /// Whether the invocation may receive the session's restricted native material: its creator,
    /// its agent or the host.
    fn native_owner(ctx: &InvocationContext, session: &StoredSession) -> bool {
        ctx.principal_kind() == PrincipalKind::Host
            || ctx.principal() == &session.created_by
            || ctx.principal() == &session.agent_id
    }

    fn start(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::StartInput,
    ) -> Result<DomainDecision, Failure> {
        let project = match &input.scope {
            DataScope::Workspace => None,
            DataScope::Project { project_id } => {
                require_project(ctx, project_id)?;
                if !tx.artifacts().project_exists(project_id)? {
                    return Err(not_found("project", &project_id.0).into());
                }
                Some(project_id.clone())
            }
        };
        if project.is_none() && ctx.project_id().is_some() {
            // A project-bound invocation cannot open a workspace (Home) session.
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "scope", None).into());
        }
        if let Some(root) = &input.root_id {
            let Some(project) = &project else {
                // A work session belongs to its root's project; name that project's scope.
                return Err(Refusal::new(DomainErrorCode::InvalidArgument, "scope", None).into());
            };
            match tx.sessions().work_root(root)? {
                Some(w) if &w.project_id == project => {}
                _ => return Err(not_found("root", &root.0).into()),
            }
        }
        // A human's Home session is conducted by the workspace coordinator; any other session by
        // the principal that started it (assignment sessions are bound by their own item).
        let agent_id = match &self.coordinator {
            Some(c)
                if ctx.principal_kind() == PrincipalKind::Human
                    && input.root_id.is_none()
                    && project.is_none() =>
            {
                if tx.actors().actor_kind(c)? != Some(PrincipalKind::Agent) {
                    return Err(not_found("coordinator", &c.0).into());
                }
                c.clone()
            }
            _ => ctx.principal().clone(),
        };
        let now = tx.now_ms();
        let session = StoredSession {
            id: tx.new_id(IdKind::Session),
            conversation_id: tx.new_id(IdKind::Conversation),
            agent_id,
            project_id: project,
            root_id: input.root_id.clone(),
            title: input.title.0.clone(),
            context_epoch: 0,
            revision: 1,
            created_by: ctx.principal().clone(),
            created_at_ms: now,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        };
        tx.sessions().insert_session(&session)?;
        let event = session_event(
            tx,
            ctx,
            &session,
            "SessionStarted",
            json!({"title": session.title, "conversationId": session.conversation_id.as_str(),
                "agentId": session.agent_id.0.as_str()}),
        )?;
        session_link(
            tx,
            ctx,
            &session,
            "session",
            None,
            event,
            format!("Session \"{}\" started", session.title),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SessionsStart {
                value: session_record(&session),
            },
        })
    }

    fn end(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::EndInput,
    ) -> Result<DomainDecision, Failure> {
        let mut session = visible_session(tx.sessions(), ctx, &input.session_id)?;
        Self::may_act(tx, ctx, &session)?;
        if input.expected_revision.get() != session.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Work,
                "session",
                session.id.clone(),
                input.expected_revision.clone(),
                rev(session.revision),
            )
            .into());
        }
        if session.closed() {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::SessionsEnd {
                    value: session_record(&session),
                },
            });
        }
        let pending = tx.sessions().pending_calls(&session.id, 500)?.len();
        session.closed_at_ms = Some(tx.now_ms());
        session.closed_by = Some(ctx.principal().clone());
        session.revision += 1;
        tx.sessions().update_session(&session)?;
        let event = session_event(
            tx,
            ctx,
            &session,
            "SessionEnded",
            json!({"lastSequence": session.last_durable_sequence(), "pendingCalls": pending}),
        )?;
        let parent = transcript::session_link(tx, &session.id)?;
        session_link(
            tx,
            ctx,
            &session,
            "session_end",
            parent,
            event,
            format!(
                "Session \"{}\" ended at transcript {} ({pending} calls without a response)",
                session.title,
                session.last_durable_sequence()
            ),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SessionsEnd {
                value: session_record(&session),
            },
        })
    }

    fn append_message(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &SessionCommandRequest,
    ) -> Result<DomainDecision, Failure> {
        let session = visible_session(tx.sessions(), ctx, &input.session_id)?;
        Self::may_act(tx, ctx, &session)?;
        if session.closed() {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                SESSION_CLOSED,
                Some(session.id.clone()),
            )
            .into());
        }
        if input.expected_session_revision.get() != session.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Work,
                "session",
                session.id.clone(),
                input.expected_session_revision.clone(),
                rev(session.revision),
            )
            .into());
        }
        let batch_id = tx.new_id(IdKind::TranscriptBatch);
        let entry_id = tx.new_id(IdKind::TranscriptEntry);
        let batch = turn::batch_of(
            &session.id,
            batch_id,
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::UserMessage {
                content: input.content.clone(),
            }],
            || entry_id.clone(),
        );
        let actor = ctx.principal().clone();
        match transcript::append_session_batch(
            tx,
            ctx,
            &actor,
            &SessionBatch::entries(batch),
            AppendPath::UserMessage,
        )? {
            BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {}
            BatchOutcome::Refused(r) => return Err(append_refusal(&r).into()),
        }
        let session = tx
            .sessions()
            .session(&session.id)?
            .ok_or_else(|| StorageError::InvalidRecord("session vanished".into()))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SessionsAppendMessage {
                value: session_record(&session),
            },
        })
    }

    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ListInput,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let scope = match &input.scope {
            DataScope::Project { project_id } => {
                require_project(ctx, project_id)?;
                if !snapshot.artifacts().project_exists(project_id)? {
                    return Err(not_found("project", &project_id.0).into());
                }
                SessionScope::Project(project_id.clone())
            }
            // Workspace scope is the Home sessions; a project-bound invocation has none.
            DataScope::Workspace => match ctx.project_id() {
                Some(_) => {
                    return Err(
                        Refusal::new(DomainErrorCode::PermissionDenied, "scope", None).into(),
                    );
                }
                None => SessionScope::Workspace,
            },
        };
        let scope_part = match &scope {
            SessionScope::All => "all".to_owned(),
            SessionScope::Project(p) => format!("project:{}", p.0.as_str()),
            SessionScope::Workspace => "workspace".to_owned(),
        };
        let closed_part = format!("{:?}", input.closed);
        let binding = CursorBinding::new(ctx, "sessions.list", &[&scope_part, &closed_part], 1);
        let after = match &input.page.cursor {
            None => None,
            Some(c) => {
                let position = binding.decode(&c.0)?;
                let (t, id) = position
                    .split_once(':')
                    .ok_or(StorageError::CursorExpired)?;
                Some(SessionPosition {
                    created_at_ms: t.parse().map_err(|_| StorageError::CursorExpired)?,
                    id: Id::new(id).map_err(|_| StorageError::CursorExpired)?,
                })
            }
        };
        let limit = input.page.limit.get() as u16;
        let rows = read.sessions(&scope, input.closed, after.as_ref(), limit)?;
        let has_more = match rows.last() {
            Some(last) if rows.len() == limit as usize => !read
                .sessions(
                    &scope,
                    input.closed,
                    Some(&SessionPosition {
                        created_at_ms: last.created_at_ms,
                        id: last.id.clone(),
                    }),
                    1,
                )?
                .is_empty(),
            _ => false,
        };
        let next_cursor = match (has_more, rows.last()) {
            (true, Some(last)) => Some(BoundedText(binding.encode(&format!(
                "{}:{}",
                last.created_at_ms,
                last.id.as_str()
            )))),
            _ => None,
        };
        Ok(OperationResult::SessionsList {
            value: Page {
                items: BoundedList(rows.iter().map(session_record).collect()),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }

    fn transcript(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        input: &SessionQueryRequest,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let session = visible_session(read, ctx, &input.session_id)?;
        let binding = CursorBinding::new(ctx, "sessions.transcript", &[session.id.as_str()], 1);
        let after = match &input.after {
            None => 0,
            Some(c) => binding
                .decode(c)?
                .parse::<i64>()
                .map_err(|_| StorageError::CursorExpired)?,
        };
        if after > session.last_durable_sequence() {
            return Err(StorageError::CursorExpired.into());
        }
        let rows = read.entries(&session.id, after, input.limit)?;
        let owner = Self::native_owner(ctx, &session);
        let mut entries = Vec::with_capacity(rows.len());
        let mut native = Vec::new();
        for e in &rows {
            // Provider-native continuation items are restricted transport material: only the
            // session's creator, its agent and the host receive their references. Everyone
            // else gets the transcript without them (a sequence gap, never altered content).
            if e.kind == "NativeItem" {
                if !owner {
                    continue;
                }
                native.push(e.sequence);
            }
            entries.push(TranscriptEntry {
                entry_id: e.entry_id.clone(),
                sequence: rev(e.sequence),
                body: serde_json::from_value(e.body.clone()).map_err(|_| {
                    StorageError::IncompatibleSchema("stored transcript entry".into())
                })?,
            });
        }
        if !native.is_empty() {
            // Every view of native material is audited before it is served; a view that cannot
            // be recorded is not served.
            self.audit_native_view(ctx, query, &session, native)?;
        }
        let last = rows.last().map_or(after, |e| e.sequence);
        let page = TranscriptPage {
            protocol: Protocol::Draft1,
            query_id: query.query_id().clone(),
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            session_id: session.id.clone(),
            entries,
            // A continuation even at the tail, so a view polls without replaying the transcript.
            next_cursor: binding.encode(&last.to_string()),
            has_more: last < session.last_durable_sequence(),
        };
        Ok(OperationResult::SessionsTranscript { value: page })
    }

    /// Recorded sources of the session's current epoch as wire context sources; omitted and
    /// truncated sources keep that disposition in their kind (`<kind>:omitted`).
    fn recorded_sources(
        read: &dyn crate::ports::storage::SessionRead,
        session: &StoredSession,
        after: i64,
        limit: u16,
    ) -> Result<Vec<(i64, ContextSource)>, Failure> {
        let mut out = Vec::new();
        for s in read.context_sources(&session.id, session.context_epoch, after, limit)? {
            let artifact = s
                .artifact
                .map(serde_json::from_value::<ArtifactRef>)
                .transpose()
                .map_err(|_| StorageError::IncompatibleSchema("stored context source".into()))?;
            let kind = match s.disposition.as_str() {
                "Included" => s.kind.clone(),
                other => format!("{}:{}", s.kind, other.to_ascii_lowercase()),
            };
            out.push((
                s.ordinal,
                ContextSource {
                    id: s.source_id,
                    kind: label(&kind, "source"),
                    source_revision: rev(s.source_revision),
                    artifact,
                    token_estimate: counter(s.token_estimate),
                },
            ));
        }
        Ok(out)
    }

    fn current_epoch(session: &StoredSession, expected: &Counter) -> Result<(), Failure> {
        if expected.get() != session.context_epoch {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                EPOCH_CHANGED,
                Some(session.id.clone()),
            )
            .retry(RetryAdvice::RefreshAndNewRequest)
            .into());
        }
        Ok(())
    }

    fn sources_binding(ctx: &InvocationContext, session: &StoredSession) -> CursorBinding {
        let epoch = session.context_epoch.to_string();
        CursorBinding::new(
            ctx,
            "sessions.context_sources",
            &[session.id.as_str(), &epoch],
            1,
        )
    }

    /// The bounded briefing of the session's next request (CONTEXT-01/02): its work item, the
    /// current epoch's immutable snapshot, the latest checkpoint (handover state), calls still
    /// waiting for a response, operations its responses wait on, then the sources recorded for
    /// the epoch in recording order — at most [`BRIEFING_LIMIT`]. When recorded sources remain,
    /// the page continues through `sessions.context_sources` with the returned cursor.
    ///
    /// Schema revision 2 (the retained `get_briefing` arguments):
    /// - `item` adds that work item (`focus:<kind>`) right after the session's own work item. It
    ///   must be visible to the invocation and belong to the session's project (any visible
    ///   project for a workspace session); otherwise it is `NotFound`, like any other work item
    ///   outside the scope. Naming the session's own work item adds nothing.
    /// - `paths` (1-128 distinct repository-relative paths) scope the path knowledge of the
    ///   briefing. The sources a briefing composes today (work items, epoch snapshot, checkpoint,
    ///   open calls and operations, the host's recorded sources) are not path-scoped and no
    ///   path-scoped knowledge is stored yet, so paths are validated and select nothing more.
    /// - `budgetTokens` caps the briefing's estimated size: the sum of its sources'
    ///   `tokenEstimate`s (the attributed estimate of the material each source names: 4 bytes
    ///   per token, rounded up, over the artifact length or work title; 0 for the references to
    ///   calls and operations, whose material is not inlined). The briefing is the longest prefix
    ///   of the order above whose sum fits; a source that does not fit ends it. When recorded
    ///   sources were left out, the cursor continues them through `sessions.context_sources`
    ///   (from the first one left out); the fixed sources a budget leaves out are re-served by a
    ///   briefing with a larger budget. No inference is involved.
    fn briefing(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::BriefingInput,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let session = visible_session(read, ctx, &input.session_id)?;
        Self::current_epoch(&session, &input.expected_context_epoch)?;
        let mut items: Vec<ContextSource> = Vec::new();
        if let Some(root) = &session.root_id
            && let Some(w) = read.work_root(root)?
        {
            items.push(ContextSource {
                id: w.id.0.clone(),
                kind: label(&format!("work:{}", w.kind), "work"),
                source_revision: rev(w.revision),
                artifact: None,
                token_estimate: counter(token_estimate(w.title.len() as i64)),
            });
        }
        if let Some(item) = &input.item
            && session.root_id.as_ref() != Some(item)
        {
            let w = visible_root(read, ctx, item)?;
            if session
                .project_id
                .as_ref()
                .is_some_and(|p| *p != w.project_id)
            {
                return Err(not_found("root", &item.0).into());
            }
            items.push(ContextSource {
                id: w.id.0.clone(),
                kind: label(&format!("focus:{}", w.kind), "focus"),
                source_revision: rev(w.revision),
                artifact: None,
                token_estimate: counter(token_estimate(w.title.len() as i64)),
            });
        }
        let artifact_source =
            |id: String, kind: &str, revision: i64, a: ArtifactRef| ContextSource {
                id: Id::new(id).expect("derived source id"),
                kind: label(kind, "source"),
                source_revision: rev(revision),
                token_estimate: counter(token_estimate(a.byte_length.get())),
                artifact: Some(a),
            };
        if session.context_epoch > 0
            && let Some(e) = read.epoch(&session.id, session.context_epoch)?
        {
            let a: ArtifactRef = serde_json::from_value(e.context)
                .map_err(|_| StorageError::IncompatibleSchema("stored epoch".into()))?;
            items.push(artifact_source(
                format!("context-epoch-{}", e.epoch),
                "context_epoch",
                e.epoch,
                a,
            ));
        }
        if let Some(c) = read.latest_checkpoint(&session.id)? {
            let a: ArtifactRef = serde_json::from_value(c.artifact)
                .map_err(|_| StorageError::IncompatibleSchema("stored checkpoint".into()))?;
            items.push(artifact_source(
                format!("checkpoint-{}", c.sequence),
                "checkpoint",
                c.sequence,
                a,
            ));
        }
        for call in read.pending_calls(&session.id, BRIEFING_OPEN_ITEMS)? {
            items.push(ContextSource {
                id: Id::new(format!("call-{}", call.call_sequence)).expect("derived source id"),
                kind: label(
                    &format!("pending_call:{}", call.capability_id),
                    "pending_call",
                ),
                source_revision: rev(call.call_sequence),
                artifact: None,
                token_estimate: counter(0),
            });
        }
        for call in read.linked_operations(&session.id, BRIEFING_OPEN_ITEMS)? {
            let Some(op) = call.operation_id else {
                continue;
            };
            let Some(record) = snapshot.operations().operation(&op)? else {
                continue;
            };
            // Only operations of the session's own scope (never another project's).
            let in_scope = match &record.root_id {
                Some(r) => read
                    .work_root(r)?
                    .is_some_and(|w| session.project_id.as_ref() == Some(&w.project_id)),
                None => session.project_id.is_none(),
            };
            if !in_scope || crate::ports::storage::is_terminal(record.state) {
                continue;
            }
            items.push(ContextSource {
                id: op.0.clone(),
                kind: label(
                    &format!("operation:{}", call.outcome.as_deref().unwrap_or("Waiting")),
                    "operation",
                ),
                source_revision: rev(record.revision),
                artifact: None,
                token_estimate: counter(0),
            });
        }
        items.truncate(BRIEFING_LIMIT);
        // The budget keeps the longest prefix whose estimated size fits (revision 2).
        let budget = input.budget_tokens.as_ref().map(|b| i64::from(b.get()));
        let mut spent: i64 = 0;
        let fits = |spent: &mut i64, s: &ContextSource| match budget {
            None => true,
            Some(b) => {
                *spent = spent.saturating_add(s.token_estimate.get());
                *spent <= b
            }
        };
        if let Some(cut) = items.iter().position(|s| !fits(&mut spent, s)) {
            items.truncate(cut);
            let next_cursor = (!Self::recorded_sources(read, &session, 0, 1)?.is_empty())
                .then(|| BoundedText(Self::sources_binding(ctx, &session).encode("0")));
            return Ok(OperationResult::SessionsBriefing {
                value: Page {
                    items: BoundedList(items),
                    has_more: next_cursor.is_some(),
                    next_cursor,
                },
            });
        }
        let room = BRIEFING_LIMIT - items.len();
        let mut next_cursor = None;
        if room > 0 {
            let recorded = Self::recorded_sources(read, &session, 0, room as u16 + 1)?;
            let more = recorded.len() > room;
            let mut taken: Vec<_> = recorded.into_iter().take(room).collect();
            let within = taken
                .iter()
                .position(|(_, s)| !fits(&mut spent, s))
                .unwrap_or(taken.len());
            // Sources the budget leaves out continue after the last one included.
            let cut = within < taken.len();
            taken.truncate(within);
            if more || cut {
                let after = taken.last().map_or(0, |(ordinal, _)| *ordinal);
                next_cursor = Some(BoundedText(
                    Self::sources_binding(ctx, &session).encode(&after.to_string()),
                ));
            }
            items.extend(taken.into_iter().map(|(_, s)| s));
        } else if !Self::recorded_sources(read, &session, 0, 1)?.is_empty() {
            next_cursor = Some(BoundedText(
                Self::sources_binding(ctx, &session).encode("0"),
            ));
        }
        Ok(OperationResult::SessionsBriefing {
            value: Page {
                items: BoundedList(items),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }

    fn context_sources(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ContextSourcesInput,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let session = visible_session(read, ctx, &input.session_id)?;
        Self::current_epoch(&session, &input.expected_context_epoch)?;
        let binding = Self::sources_binding(ctx, &session);
        let after = match &input.page.cursor {
            None => 0,
            Some(c) => binding
                .decode(&c.0)?
                .parse::<i64>()
                .map_err(|_| StorageError::CursorExpired)?,
        };
        let limit = input.page.limit.get() as u16;
        let rows = Self::recorded_sources(read, &session, after, limit)?;
        let has_more = match rows.last() {
            Some((ordinal, _)) if rows.len() == limit as usize => {
                !Self::recorded_sources(read, &session, *ordinal, 1)?.is_empty()
            }
            _ => false,
        };
        let next_cursor = match (has_more, rows.last()) {
            (true, Some((ordinal, _))) => Some(BoundedText(binding.encode(&ordinal.to_string()))),
            _ => None,
        };
        Ok(OperationResult::SessionsContextSources {
            value: Page {
                items: BoundedList(rows.into_iter().map(|(_, s)| s).collect()),
                has_more: next_cursor.is_some(),
                next_cursor,
            },
        })
    }
}

impl QueryHandler for SessionService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Sessions(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let answered = match input {
            ops::Input::List(i) => self.list(snapshot, ctx, i),
            ops::Input::Get(i) => {
                visible_session(snapshot.sessions(), ctx, &i.session_id).map(|s| {
                    OperationResult::SessionsGet {
                        value: session_record(&s),
                    }
                })
            }
            ops::Input::Transcript(i) => self.transcript(snapshot, ctx, query, i),
            ops::Input::Briefing(i) => self.briefing(snapshot, ctx, i),
            ops::Input::ContextSources(i) => self.context_sources(snapshot, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let result = answered.map_err(|f| query_fault(f, "session", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for SessionService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Sessions(ops::Input::Start(i)) => self.start(tx, ctx, i),
            OperationInput::Sessions(ops::Input::End(i)) => self.end(tx, ctx, i),
            OperationInput::Sessions(ops::Input::AppendMessage(i)) => {
                self.append_message(tx, ctx, i)
            }
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        decided(tx, outcome)
    }
}
