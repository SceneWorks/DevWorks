//! `HistoryService`: the work history record and relationship graph behind `history.events`,
//! `.work_lineage`, `.git_links`, `.evidence_links`, `.operation_status`, `.tool_usage` and
//! `.command_receipt`, registered through [`crate::services::UseCases::with_history`]
//! (HISTORY-01/02).
//!
//! # Record and graph
//!
//! History is the append-only event record plus typed links (`history_links`) anchored at work
//! roots, both written atomically with the facts they describe: sessions and their ends, durable
//! tool calls and initial responses, context epochs, checkpoints and operation updates (by the
//! session writer), and Git/PR, review, decision, outcome and evidence links recorded through
//! [`record_link`] by the items that observe them. Every link names its actor, work scope,
//! creating event (committed sequence and time) and, where relevant, repository and revision.
//! A correction or reversal is a new link that names the link it supersedes; the prior record is
//! never changed or removed. Commands decided in a work scope and artifact facts are attached to
//! the root by their events, so lineage navigates from work to sessions, assignments' sessions,
//! tool actions, Git/PR links, evidence and artifacts, and the graph's reverse index navigates
//! from a session, call or commit back to its work (`SessionRead::links_to`).
//!
//! # Queries
//!
//! Every page is bounded (1..=500), ordered by committed sequence (gaps are normal) and checked
//! against the invocation's project scope: a root or project outside it is `NotFound`. Cursors
//! are versioned and bound to the query, its scope (root or project), principal, project, store,
//! restore epoch, the scope's revision and, from schema revision 2, the negotiated revision; any
//! change expires them (`CursorExpired`). The gateway re-checks the authorization's fences on a
//! fresh snapshot before a page is published, and publishes it under the negotiated revision's
//! projection.
//!
//! `history.events` publishes the facts the recorded `Event` contract can type: the
//! `CommandDecided` decision of a command, with its real subject — the work item it targeted or
//! the durable operation it names — at the subject's revision after the decision. At revision 1
//! a root's feed has only those decisions; a decision attached to the root only by the invoking
//! agent's lineage has no aggregate the revision-1 `Subject` union can express, so it appears in
//! `history.work_lineage` instead. From revision 2 the aggregate may be the command itself
//! (`EventSubject::Command`, revision kind `Command`, revision 1), so a root's feed has every
//! decision attached to it and `projectId` selects the project change feed: every command decided
//! in that project (the gateway attaches a workspace command naming a visible project to it), in
//! committed-sequence order, visible to the principals that may see the project. The gateway
//! refuses to publish a command aggregate below revision 2.
//!
//! Revision 3 adds `newestFirst` to `history.events`: its first page reads the newest decisions
//! and its cursor continues strictly backward. Direction is part of the cursor binding; it
//! cannot be exchanged with the legacy forward feed. Older revisions remain forward-only.
//!
//! From revision 2, every history page (`HistoryPage`, `UsagePage`) carries `checkpoint`, the
//! continuation after the last fact it covers, including at the current tail (an empty page keeps
//! the position it was asked from), so a poller resumes without replaying the history; while
//! `hasMore` it equals `nextCursor`. Revision-1 pages keep the recorded `Page` rule (no
//! continuation at the tail). From revision 2, lineage and evidence records carry their actor,
//! time (UTC ms), repository and repository revision as fields, and Git links their actor and
//! time (the repository and head are already fields); the summaries keep the same text at every
//! revision. A related work item records no creator, so its actor is absent; its time is its
//! creation. Facts with no repository (sessions, commands, artifacts, evidence) have none.
//!
//! `history.tool_usage` revision 2 names a root, a project or the workspace (Home sessions; only a
//! workspace invocation of a principal authorized for the workspace) and interleaves tool
//! invocations with the inference usage observations, in committed-sequence order, each list with
//! its committed sequences: one `ToolInvocation` per completed invocation recorded for any session
//! (`tool_invocations`, with or without a root) — the tool (capability) name, outcome, the call and session,
//! the operation a wait or unknown outcome is linked to, the actor and time of the response, the
//! interval between the durable call and response records (`durationMs`; 0 when one batch carries
//! both) and, for a failure, its error code after redaction. Tool arguments and results are never
//! included. A call without a response is not yet an invocation (the briefing names it pending).
//!
//! `history.command_receipt` looks up the caller's own receipt and, once its durable typed reply
//! is published after commit ([`crate::services::artifacts::replies`]), returns the `Recorded`
//! observation with that reply artifact and schema. Until then it is typed `ViewUnavailable`:
//! pending (retry the same query), held behind a delivery an operator resolves (retry after that
//! blocker clears) or abandoned (never retried). A caller that can no longer see the reply is
//! denied.
//!
//! # Retained CodeGraph behavior
//!
//! `WorkController.Changes`, `WorkViewsController.ItemChanges` and the `changes` work tool with an
//! item map to `history.events` of the item; `WorkViewsController.RecentChanges` and `changes`
//! without an item map to the revision-2 project change feed (`projectId`), every write being a
//! decided command. `WorkViewsController.GitHubEvidence` maps to `history.git_links`.
//! `AdminMcpHubController.Audit`, the `AdminReportsController` reports and the
//! `McpToolInvocationRecordedConsumer` map to `history.tool_usage`: inference usage at every
//! revision, tool invocations (tool name, outcome, duration, error) from revision 2. Price
//! classification, budgets and report filters are the usage item's (`router.usage`).
use crate::authority::redact::Redactor;
use crate::context::{InvocationContext, ReceiptIdentity};
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, history as ops};
use crate::ports::storage::{
    HistoryScope, LedgerEntry, LineageRow, NewEvent, ReadSnapshot, SequenceKey, StorageError,
    StoredInvocation, StoredLink, UsageFact, WorkRoot, WriteTransaction,
};
use crate::ports::{IdKind, IdSource, QueryAnswer, QueryHandler};
use crate::protocol::data::GitOid;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AttributedGitLink, EvidenceLink, LineageRecord, OperationRef, ReceiptLookupState,
    ReceiptObservation, ToolInvocation, ToolInvocationOutcome, UsageItem, UsagePage,
};
use crate::protocol::results::OperationResult;
use crate::protocol::shared::{Event, EventBody, EventRevisionKind, EventSubject, Query};
use crate::protocol::support::{BoundedList, BoundedText, HistoryPage, Text, Url};
use crate::protocol::{
    ActorId, CommandId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, OperationId,
    ProjectId, Protocol, RequestKey, RetryAdvice, RootId,
};
use crate::registry::SchemaRevision;
use crate::services::artifacts::Viewer;
use crate::services::artifacts::replies::{ObservationGap, recorded_observation};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{
    Failure, Refusal, content, counter, label, not_found, project_visible, query_fault, rev,
    transcript::reference_resolves, visible_root,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// The operations [`HistoryService`] is registered for.
pub const OPERATIONS: [&str; 7] = [
    "history.events",
    "history.work_lineage",
    "history.git_links",
    "history.evidence_links",
    "history.operation_status",
    "history.tool_usage",
    "history.command_receipt",
];
/// History aggregate of a recorded link (`aggregate_id` = link ID).
pub const LINK_AGGREGATE: &str = "history_link";
/// Fault target kind of a recorded receipt whose durable reply artifact is still being published
/// (`ViewUnavailable`, retry the same query) or is no longer visible to the caller
/// (`PermissionDenied`).
pub const RECEIPT_REPLY_UNAVAILABLE: &str = "receipt_reply_artifact";
/// Fault target kind of a recorded receipt whose reply publication is held or failed: the target
/// is the delivery an operator resolves (`ViewUnavailable`, retry after that blocker clears).
pub const RECEIPT_REPLY_HELD: &str = "receipt_reply_held";
/// Fault target kind of a recorded receipt whose reply will never be published (its delivery was
/// cancelled or dismissed, or no intent exists): `ViewUnavailable`, never retried.
pub const RECEIPT_REPLY_ABANDONED: &str = "receipt_reply_abandoned";

/// Link kinds other items record through [`record_link`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Git,
    PullRequest,
    Review,
    Decision,
    Outcome,
    Evidence,
    /// A reversal of the superseded link with no replacement.
    Correction,
}
impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::PullRequest => "pull_request",
            Self::Review => "review",
            Self::Decision => "decision",
            Self::Outcome => "outcome",
            Self::Evidence => "evidence",
            Self::Correction => "correction",
        }
    }
}

/// The Git facts of a `git`/`pull_request` link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFacts {
    pub repository_id: Id,
    pub worktree_id: Option<Id>,
    pub branch: Option<String>,
    pub pull_request_url: Option<String>,
    pub head: GitOid,
}

/// What another item records about a work root.
#[derive(Clone, Debug)]
pub struct NewHistoryLink {
    pub root_id: RootId,
    pub kind: LinkKind,
    /// Required for `Git`/`PullRequest`.
    pub git: Option<GitFacts>,
    /// Required for `Evidence`: the immutable evidence artifact version.
    pub artifact: Option<ArtifactRef>,
    /// `(target kind, target ID)` the link points at. Required for `Review`, `Decision` and
    /// `Outcome` (a review run, decision record, ...); ignored for the other kinds, whose target
    /// is fixed: `Git`/`PullRequest` target the head commit, `Evidence` the artifact version and
    /// `Correction` the superseded link.
    pub target: Option<(String, String)>,
    pub summary: String,
    /// The earlier link this one corrects or reverses (it stays unchanged).
    pub supersedes: Option<Id>,
}

/// Records one link on a work root inside the caller's unit of work, with its attributed
/// `HistoryLinkRecorded` event. The root must be visible to the invocation; a superseded link
/// must belong to the same root (and, unless this is a `Correction`, be of the same kind).
pub fn record_link(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    new: NewHistoryLink,
) -> Result<Result<StoredLink, Refusal>, StorageError> {
    let Some(root) = tx.sessions().work_root(&new.root_id)? else {
        return Ok(Err(not_found("root", &new.root_id.0)));
    };
    if !project_visible(ctx, Some(&root.project_id)) {
        return Ok(Err(not_found("root", &new.root_id.0)));
    }
    let invalid = |kind| {
        Ok(Err(Refusal::new(
            DomainErrorCode::InvalidArgument,
            kind,
            None,
        )))
    };
    if new.summary.is_empty() || new.summary.len() > 4096 || new.summary.contains('\0') {
        return invalid("summary");
    }
    if let Some(prior) = &new.supersedes {
        match tx.sessions().link(prior)? {
            Some(p)
                if p.root_id == root.id
                    && (new.kind == LinkKind::Correction || p.kind == new.kind.as_str()) => {}
            _ => return Ok(Err(not_found("history_link", prior))),
        }
    } else if new.kind == LinkKind::Correction {
        return invalid("supersedes");
    }
    let (target_kind, target_id, repository_id, revision, body) = match new.kind {
        LinkKind::Git | LinkKind::PullRequest => {
            let Some(g) = &new.git else {
                return invalid("git");
            };
            if g.head.validate().is_err()
                || g.branch
                    .as_ref()
                    .is_some_and(|b| b.is_empty() || b.len() > 4096)
                || g.pull_request_url
                    .as_ref()
                    .is_some_and(|u| serde_json::from_value::<Url>(json!(u)).is_err())
                || (new.kind == LinkKind::PullRequest && g.pull_request_url.is_none())
            {
                return invalid("git");
            }
            // The repository must be a member of the work's project.
            if !tx
                .sessions()
                .project_has_repository(&root.project_id, &g.repository_id)?
            {
                return Ok(Err(not_found("repository", &g.repository_id)));
            }
            (
                "commit".to_owned(),
                g.head.hex.clone(),
                Some(g.repository_id.clone()),
                Some(g.head.hex.clone()),
                json!({"repositoryId": g.repository_id.as_str(),
                    "worktreeId": g.worktree_id.as_ref().map(Id::as_str),
                    "branch": g.branch, "pullRequestUrl": g.pull_request_url,
                    "head": serde_json::to_value(&g.head).unwrap_or(Value::Null)}),
            )
        }
        LinkKind::Evidence => {
            let Some(a) = &new.artifact else {
                return invalid("artifact");
            };
            if !reference_resolves(tx, ctx, Some(&root.project_id), a)? {
                return Ok(Err(not_found("artifact_version", &a.version_id)));
            }
            (
                "artifact_version".to_owned(),
                a.version_id.as_str().to_owned(),
                None,
                None,
                json!({"artifact": serde_json::to_value(a).unwrap_or(Value::Null)}),
            )
        }
        LinkKind::Correction => {
            let prior = new.supersedes.as_ref().expect("checked above");
            (
                "history_link".to_owned(),
                prior.as_str().to_owned(),
                None,
                None,
                json!({}),
            )
        }
        LinkKind::Review | LinkKind::Decision | LinkKind::Outcome => {
            let Some((kind, id)) = &new.target else {
                return invalid("target");
            };
            if kind.is_empty() || kind.len() > 64 || id.is_empty() || id.len() > 1024 {
                return invalid("target");
            }
            (kind.clone(), id.clone(), None, None, json!({}))
        }
    };
    let id = tx.new_id(IdKind::Link);
    let event_id = EventId(tx.new_id(IdKind::Event));
    let seq = tx.history().append(NewEvent {
        id: event_id.clone(),
        actor: ctx.principal().clone(),
        project_id: Some(root.project_id.clone()),
        root_id: Some(root.id.clone()),
        operation_id: None,
        aggregate_kind: LINK_AGGREGATE.into(),
        aggregate_id: id.as_str().to_owned(),
        aggregate_revision: 1,
        kind: "HistoryLinkRecorded".into(),
        payload: json!({"kind": new.kind.as_str(), "targetKind": target_kind, "targetId": target_id,
            "supersedes": new.supersedes.as_ref().map(Id::as_str),
            "repositoryId": repository_id.as_ref().map(Id::as_str), "revision": revision,
            "principalKind": format!("{:?}", ctx.principal_kind()),
            "sessionId": ctx.session_id().map(Id::as_str)}),
    })?;
    let link = StoredLink {
        id,
        project_id: root.project_id.clone(),
        root_id: root.id.clone(),
        kind: new.kind.as_str().to_owned(),
        target_kind,
        target_id,
        parent_id: None,
        supersedes_id: new.supersedes,
        actor: ctx.principal().clone(),
        event_id: event_id.0,
        sequence: seq,
        repository_id,
        revision,
        summary: new.summary,
        body,
        created_at_ms: tx.now_ms(),
    };
    tx.sessions().insert_link(&link)?;
    Ok(Ok(link))
}

/// A lineage summary with the attribution (HISTORY-02): what happened, by whom, when (UTC ms)
/// and at which committed sequence. Revision 2 also carries the attribution as fields.
fn attributed(what: &str, actor: &ActorId, at_ms: i64, sequence: i64) -> String {
    format!(
        "{what} - by {} at {at_ms} (sequence {sequence})",
        actor.0.as_str()
    )
}

/// The recorded subject of a decision (`payload.subject`): a work item at its work revision or
/// an operation at its operation revision. `None` when the command named neither.
fn decision_subject(
    payload: &Value,
) -> Result<Option<(EventSubject, EventRevisionKind, i64)>, Failure> {
    let s = &payload["subject"];
    if s.is_null() {
        return Ok(None);
    }
    let id = s["id"]
        .as_str()
        .and_then(|i| Id::new(i).ok())
        .ok_or_else(|| stored("decision subject"))?;
    let revision = s["revision"]
        .as_i64()
        .filter(|r| *r > 0)
        .ok_or_else(|| stored("decision subject revision"))?;
    match (s["kind"].as_str(), s["dimension"].as_str()) {
        (Some("root"), Some("Work")) => Ok(Some((
            EventSubject::Root(RootId(id)),
            EventRevisionKind::Work,
            revision,
        ))),
        (Some("operation"), Some("Operation")) => Ok(Some((
            EventSubject::Operation(OperationId(id)),
            EventRevisionKind::Operation,
            revision,
        ))),
        _ => Err(stored("decision subject kind")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitBody {
    worktree_id: Option<Id>,
    branch: Option<String>,
    pull_request_url: Option<String>,
    agent_session_id: Option<Value>,
    head: GitOid,
}

fn stored(what: &str) -> Failure {
    StorageError::IncompatibleSchema(format!("stored {what}")).into()
}

fn key_position(k: &SequenceKey) -> String {
    format!("{}:{}:{}", k.sequence, k.source, k.id)
}
fn parse_key(position: &str) -> Result<SequenceKey, StorageError> {
    let mut parts = position.splitn(3, ':');
    let (Some(seq), Some(src), Some(id)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(StorageError::CursorExpired);
    };
    Ok(SequenceKey {
        sequence: seq.parse().map_err(|_| StorageError::CursorExpired)?,
        source: src.parse().map_err(|_| StorageError::CursorExpired)?,
        id: id.to_owned(),
    })
}
/// The position before every fact: the start of a history.
fn start() -> SequenceKey {
    SequenceKey {
        sequence: -1,
        source: 0,
        id: String::new(),
    }
}

fn text_of(what: &str, s: &str) -> Result<Text, Failure> {
    serde_json::from_value::<Text>(json!(s)).map_err(|_| stored(what))
}

pub struct HistoryService {
    ids: Arc<dyn IdSource>,
}

/// One bounded page of rows with their continuation.
struct Paged<T> {
    items: Vec<T>,
    next: Option<String>,
    /// The continuation after the last fact the page covers (revision 2 `checkpoint`).
    checkpoint: String,
}
impl<T> Paged<T> {
    fn history(self) -> HistoryPage<T> {
        HistoryPage {
            items: BoundedList(self.items),
            has_more: self.next.is_some(),
            next_cursor: self.next.map(BoundedText),
            checkpoint: Some(BoundedText(self.checkpoint)),
        }
    }
}

/// What a `history.events` page covers.
enum EventScope<'a> {
    Root(&'a RootId),
    Project(&'a ProjectId),
}

impl HistoryService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Binds `op` over `parts` at `scope_revision`. From revision 2 the negotiated revision is
    /// bound too, since a revision-2 page covers facts a revision-1 page does not.
    fn bind(
        ctx: &InvocationContext,
        op: &str,
        parts: &[&str],
        scope_revision: i64,
        revision: SchemaRevision,
    ) -> CursorBinding {
        let negotiated = format!("r{}", revision.0);
        let mut parts = parts.to_vec();
        if revision.0 >= 2 {
            parts.push(&negotiated);
        }
        CursorBinding::new(ctx, op, &parts, scope_revision)
    }
    fn binding(
        ctx: &InvocationContext,
        op: &str,
        root: &WorkRoot,
        revision: SchemaRevision,
    ) -> CursorBinding {
        Self::bind(
            ctx,
            op,
            &[root.id.0.as_str()],
            root.scope_revision,
            revision,
        )
    }

    /// Pages `fetch(after, limit)` keyed rows: `limit` rows plus a probe for one more.
    fn page<T>(
        binding: &CursorBinding,
        cursor: Option<&str>,
        limit: u16,
        fetch: impl Fn(Option<&SequenceKey>, u16) -> Result<Vec<(SequenceKey, T)>, Failure>,
    ) -> Result<Paged<T>, Failure> {
        let after = cursor
            .map(|c| binding.decode(c).and_then(|p| parse_key(&p)))
            .transpose()?;
        let rows = fetch(after.as_ref(), limit)?;
        let next = match rows.last() {
            Some((last, _)) if rows.len() == limit as usize => {
                (!fetch(Some(last), 1)?.is_empty()).then(|| binding.encode(&key_position(last)))
            }
            _ => None,
        };
        let covered = rows
            .last()
            .map(|(k, _)| k.clone())
            .or(after)
            .unwrap_or_else(start);
        Ok(Paged {
            items: rows.into_iter().map(|(_, t)| t).collect(),
            next,
            checkpoint: binding.encode(&key_position(&covered)),
        })
    }

    /// One page of `history.events`; also the scope revision the page was read under.
    #[allow(clippy::too_many_arguments)]
    fn events(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: EventScope<'_>,
        after: Option<&str>,
        limit: u16,
        newest_first: bool,
        revision: SchemaRevision,
    ) -> Result<(OperationResult, i64), Failure> {
        let read = snapshot.sessions();
        let (binding, scope, scope_revision) = match scope {
            EventScope::Root(root_id) => {
                let root = visible_root(read, ctx, root_id)?;
                (
                    Self::bind(
                        ctx,
                        "history.events",
                        &if newest_first {
                            vec![root.id.0.as_str(), "newest-first"]
                        } else {
                            vec![root.id.0.as_str()]
                        },
                        root.scope_revision,
                        revision,
                    ),
                    HistoryScope::Root(root.id.clone()),
                    root.scope_revision,
                )
            }
            EventScope::Project(project) => {
                // The project feed: a project outside the invocation's scope does not exist for
                // it, exactly like a root outside it.
                let scope_revision = match read.project_scope_revision(project)? {
                    Some(r) if project_visible(ctx, Some(project)) => r,
                    _ => return Err(not_found("project", &project.0).into()),
                };
                (
                    Self::bind(
                        ctx,
                        "history.events",
                        &if newest_first {
                            vec!["project", project.0.as_str(), "newest-first"]
                        } else {
                            vec!["project", project.0.as_str()]
                        },
                        scope_revision,
                        revision,
                    ),
                    HistoryScope::Project(project.clone()),
                    scope_revision,
                )
            }
        };
        let paged = Self::page(&binding, after, limit, |after, n| {
            let from = after.map_or(0, |k| k.sequence.max(0));
            if from > snapshot.stamp().committed_sequence {
                return Err(StorageError::CursorExpired.into());
            }
            let rows = if newest_first {
                read.decisions_before(&scope, after.map(|_| from), n)?
            } else {
                match &scope {
                    // Revision 1 publishes only the decisions its `Subject` union can type.
                    HistoryScope::Root(root) if revision.0 < 2 => {
                        read.root_events(root, &["CommandDecided"], from, n)?
                    }
                    scope => read.decisions(scope, from, n)?,
                }
            };
            rows.into_iter()
                .map(|e| {
                    let key = SequenceKey {
                        sequence: e.sequence,
                        source: 2,
                        id: e.id.0.as_str().to_owned(),
                    };
                    let command = CommandId(
                        Id::new(e.aggregate_id.clone()).map_err(|_| stored("command event"))?,
                    );
                    // The decision's real subject at its revision after the decision, or the
                    // command itself (revision 2) when the command named none.
                    let (aggregate, kind, aggregate_revision) = match decision_subject(&e.payload)?
                    {
                        Some(subject) => subject,
                        None if revision.0 >= 2 => (
                            EventSubject::Command(command.clone()),
                            EventRevisionKind::Command,
                            1,
                        ),
                        None => return Err(stored("decision subject")),
                    };
                    Ok((
                        key,
                        Event {
                            protocol: Protocol::Draft1,
                            event_id: e.id.clone(),
                            sequence: rev(e.sequence),
                            aggregate,
                            aggregate_revision: rev(aggregate_revision),
                            aggregate_revision_kind: kind,
                            actor_id: e.actor.clone(),
                            occurred_at_ms: counter(e.created_at_ms),
                            command_id: Some(command.clone()),
                            project_id: e.project_id.clone(),
                            root_id: e.root_id.clone(),
                            event: EventBody::CommandDecided {
                                command_id: command,
                            },
                        },
                    ))
                })
                .collect()
        })?;
        Ok((
            OperationResult::HistoryEvents {
                value: paged.history(),
            },
            scope_revision,
        ))
    }

    fn lineage_record(row: LineageRow, root: &WorkRoot) -> Result<LineageRecord, Failure> {
        Ok(match row {
            LineageRow::Work { work, .. } => {
                let parent = root.parent_feature_id.as_ref() == Some(&work.id);
                LineageRecord {
                    id: work.id.0.clone(),
                    parent_id: None,
                    kind: label(if parent { "parent_feature" } else { &work.kind }, "work"),
                    related_id: work.id.0.clone(),
                    summary: content(&format!(
                        "{} ({} in project {}, work revision {})",
                        work.title,
                        work.kind,
                        work.project_id.0.as_str(),
                        work.revision
                    )),
                    // Work items record no creator; the time is the item's creation.
                    actor_id: None,
                    occurred_at_ms: Some(counter(work.created_at_ms)),
                    repository_id: None,
                    revision: None,
                }
            }
            LineageRow::Link { link, .. } => {
                let related = Id::new(link.target_id.clone()).unwrap_or_else(|_| link.id.clone());
                let mut what = match &link.supersedes_id {
                    Some(prior) => format!("Supersedes {}: {}", prior.as_str(), link.summary),
                    None => link.summary.clone(),
                };
                if let Some(repo) = &link.repository_id {
                    what.push_str(&format!(" [repository {}", repo.as_str()));
                    if let Some(r) = &link.revision {
                        what.push_str(&format!(" at {r}"));
                    }
                    what.push(']');
                }
                LineageRecord {
                    id: link.id.clone(),
                    parent_id: link.supersedes_id.clone().or(link.parent_id.clone()),
                    kind: label(&link.kind, "link"),
                    related_id: related,
                    summary: content(&attributed(
                        &what,
                        &link.actor,
                        link.created_at_ms,
                        link.sequence,
                    )),
                    actor_id: Some(link.actor.clone()),
                    occurred_at_ms: Some(counter(link.created_at_ms)),
                    revision: link
                        .revision
                        .as_deref()
                        .map(|r| text_of("link revision", r))
                        .transpose()?,
                    repository_id: link.repository_id,
                }
            }
            LineageRow::Event { event, .. } => {
                let (kind, what) = match event.aggregate_kind.as_str() {
                    "command" => (
                        "command",
                        format!(
                            "{} {}",
                            event.payload["operation"].as_str().unwrap_or("command"),
                            event.payload["decision"].as_str().unwrap_or("")
                        ),
                    ),
                    _ => ("artifact", format!("{} {}", event.kind, event.aggregate_id)),
                };
                let what = match event.operation_id.as_ref() {
                    Some(op) => format!("{what} (operation {})", op.0.as_str()),
                    None => what,
                };
                LineageRecord {
                    id: event.id.0.clone(),
                    parent_id: None,
                    kind: label(kind, "event"),
                    related_id: Id::new(event.aggregate_id.clone())
                        .map_err(|_| stored("lineage event"))?,
                    summary: content(&attributed(
                        &what,
                        &event.actor,
                        event.created_at_ms,
                        event.sequence,
                    )),
                    actor_id: Some(event.actor.clone()),
                    occurred_at_ms: Some(counter(event.created_at_ms)),
                    repository_id: None,
                    revision: None,
                }
            }
        })
    }

    fn work_lineage(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::WorkLineageInput,
        revision: SchemaRevision,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, &input.root_id)?;
        // Related work rows have fixed keys at the start of the order, so a hierarchy change
        // (work added, moved in or out) expires the cursor visibly instead of skipping it.
        let membership = read.work_membership(&root.id)?;
        let binding = Self::bind(
            ctx,
            "history.work_lineage",
            &[root.id.0.as_str(), &membership],
            root.scope_revision,
            revision,
        );
        let paged = Self::page(
            &binding,
            input.page.cursor.as_ref().map(|c| c.0.as_str()),
            input.page.limit.get() as u16,
            |after, n| {
                read.lineage(&root.id, after, n)?
                    .into_iter()
                    .map(|r| {
                        let key = r.key().clone();
                        Ok((key, Self::lineage_record(r, &root)?))
                    })
                    .collect()
            },
        )?;
        Ok(OperationResult::HistoryWorkLineage {
            value: paged.history(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn links_page<T>(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        op: &str,
        root_id: &RootId,
        kinds: &[&str],
        page: &crate::protocol::records::PageRequest,
        revision: SchemaRevision,
        map: impl Fn(&WorkRoot, StoredLink) -> Result<T, Failure>,
    ) -> Result<HistoryPage<T>, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, root_id)?;
        let binding = Self::binding(ctx, op, &root, revision);
        Self::page(
            &binding,
            page.cursor.as_ref().map(|c| c.0.as_str()),
            page.limit.get() as u16,
            |after, n| {
                read.links(&root.id, kinds, after, n)?
                    .into_iter()
                    .map(|l| {
                        let key = SequenceKey {
                            sequence: l.sequence,
                            source: 1,
                            id: l.id.as_str().to_owned(),
                        };
                        Ok((key, map(&root, l)?))
                    })
                    .collect()
            },
        )
        .map(Paged::history)
    }

    fn operation_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation_id: &OperationId,
    ) -> Result<OperationResult, Failure> {
        let Some(op) = snapshot.operations().operation(operation_id)? else {
            return Err(not_found("operation", &operation_id.0).into());
        };
        let visible = match &op.root_id {
            Some(root) => snapshot
                .sessions()
                .work_root(root)?
                .is_some_and(|w| project_visible(ctx, Some(&w.project_id))),
            None => ctx.project_id().is_none(),
        };
        if !visible {
            return Err(not_found("operation", &operation_id.0).into());
        }
        Ok(OperationResult::HistoryOperationStatus {
            value: OperationRef {
                operation_id: op.id,
                state: op.state,
            },
        })
    }

    /// The caller's own receipt under its host-bound receipt identity (actor, scope key, request
    /// key): another principal's or another ingress namespace's receipt is never found here.
    fn command_receipt(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        request_key: &RequestKey,
        revision: SchemaRevision,
    ) -> Result<OperationResult, Failure> {
        let identity = ReceiptIdentity::bind(ctx, request_key.clone());
        match snapshot.receipts().lookup(&identity)? {
            None => Ok(OperationResult::HistoryCommandReceipt {
                value: ReceiptObservation {
                    request_key: request_key.clone(),
                    command_id: None,
                    committed_sequence: None,
                    operation_id: None,
                    reply_artifact: None,
                    reply_schema: None,
                    state: ReceiptLookupState::NotFound,
                    purged: None,
                },
            }),
            Some(LedgerEntry::Purged(stored)) => {
                let _viewer = Viewer::resolve(snapshot.authority(), ctx)?;
                if revision.0 < 6 {
                    return Err(Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        "receipt_content_purged",
                        Some(stored.outcome.command_id.0),
                    )
                    .into());
                }
                let outcome = stored.outcome;
                Ok(OperationResult::HistoryCommandReceipt {
                    value: ReceiptObservation {
                        request_key: request_key.clone(),
                        command_id: Some(outcome.command_id.clone()),
                        committed_sequence: Some(outcome.committed_sequence.clone()),
                        operation_id: outcome.operation_id.clone(),
                        reply_artifact: None,
                        reply_schema: None,
                        state: ReceiptLookupState::Recorded,
                        purged: Some(outcome),
                    },
                })
            }
            Some(LedgerEntry::Current(stored)) => {
                let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
                match recorded_observation(snapshot, &viewer, &identity, &stored)? {
                    Ok(value) => Ok(OperationResult::HistoryCommandReceipt { value }),
                    // The recorded rules require a `Recorded` observation to carry its durable
                    // typed reply and forbid a reply on any other state; `StatusUnknown` is local
                    // admission uncertainty (a lost commit acknowledgment), and this receipt is
                    // committed. While its reply is being published after commit the view is
                    // typed as unavailable for now: the same query is retried. Nothing is
                    // guessed, re-run or changed.
                    Err(ObservationGap::Pending) => Err(Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        RECEIPT_REPLY_UNAVAILABLE,
                        Some(stored.receipt.command_id.0),
                    )
                    .retry(RetryAdvice::SameCommand)
                    .into()),
                    // Publication waits behind a held or failed delivery: retrying the query
                    // changes nothing until that delivery (named here) is resolved.
                    Err(ObservationGap::Held(delivery)) => Err(Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        RECEIPT_REPLY_HELD,
                        Some(delivery),
                    )
                    .retry(RetryAdvice::AfterBlockerClears)
                    .into()),
                    // The reply will never be published: no retry is advised.
                    Err(ObservationGap::Abandoned) => Err(Refusal::new(
                        DomainErrorCode::ViewUnavailable,
                        RECEIPT_REPLY_ABANDONED,
                        Some(stored.receipt.command_id.0),
                    )
                    .into()),
                    // Access is rechecked before an old receipt is published: a caller that can
                    // no longer see its reply is denied rather than handed it.
                    Err(ObservationGap::Denied) => Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        RECEIPT_REPLY_UNAVAILABLE,
                        Some(stored.receipt.command_id.0),
                    )
                    .into()),
                }
            }
            Some(LedgerEntry::Incompatible { .. }) => {
                Err(Refusal::new(DomainErrorCode::SchemaMismatch, "command_receipt", None).into())
            }
        }
    }
}

fn git_link(
    snapshot: &dyn ReadSnapshot,
    root: &WorkRoot,
    l: StoredLink,
    revision: SchemaRevision,
) -> Result<AttributedGitLink, Failure> {
    let body: GitBody = serde_json::from_value(l.body).map_err(|_| stored("git link"))?;
    let branch = body
        .branch
        .map(|b| serde_json::from_value::<Text>(json!(b)))
        .transpose()
        .map_err(|_| stored("git branch"))?;
    let url = body
        .pull_request_url
        .map(|u| serde_json::from_value::<Url>(json!(u)))
        .transpose()
        .map_err(|_| stored("pull request URL"))?;
    let repository_id = l.repository_id.ok_or_else(|| stored("git repository"))?;
    let tree = if revision >= SchemaRevision(3) {
        body.worktree_id
            .as_ref()
            .map(|id| snapshot.git().git_worktree(id))
            .transpose()?
            .flatten()
            .filter(|t| t.repository_id == repository_id)
    } else {
        None
    };
    let tree = match tree {
        Some(tree)
            if snapshot
                .work()
                .work(&tree.work_id)?
                .is_some_and(|work| work.project_id == root.project_id) =>
        {
            Some(tree)
        }
        _ => None,
    };
    let repository_url = if revision >= SchemaRevision(3) {
        snapshot
            .projects()
            .repository(&repository_id)?
            .and_then(|r| r.remote_url)
            .and_then(|url| crate::services::git::github::Repository::from_remote(&url))
            .and_then(|r| {
                serde_json::from_value(json!(format!("https://github.com/{}/{}", r.owner, r.name)))
                    .ok()
            })
    } else {
        None
    };
    Ok(AttributedGitLink {
        repository_id,
        worktree_path: tree
            .as_ref()
            .map(|t| text_of("worktree path", &t.path))
            .transpose()?,
        worktree_removed: tree.as_ref().map(|t| t.removed),
        agent_session_id: if revision >= SchemaRevision(3) {
            body.agent_session_id
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| stored("git agent session"))?
        } else {
            None
        },
        repository_url,
        work_id: root.id.0.clone(),
        worktree_id: body.worktree_id,
        branch,
        pull_request_url: url,
        head: body.head,
        actor_id: Some(l.actor),
        occurred_at_ms: Some(counter(l.created_at_ms)),
    })
}

fn evidence_link(_: &WorkRoot, l: StoredLink) -> Result<EvidenceLink, Failure> {
    let artifact: ArtifactRef =
        serde_json::from_value(l.body["artifact"].clone()).map_err(|_| stored("evidence"))?;
    let summary = match &l.supersedes_id {
        Some(prior) => format!("Supersedes {}: {}", prior.as_str(), l.summary),
        None => l.summary,
    };
    Ok(EvidenceLink {
        id: l.id,
        target_id: artifact.version_id.clone(),
        artifact,
        summary: content(&summary),
        actor_id: Some(l.actor),
        occurred_at_ms: Some(counter(l.created_at_ms)),
        revision: l
            .revision
            .as_deref()
            .map(|r| text_of("evidence revision", r))
            .transpose()?,
        repository_id: l.repository_id,
    })
}

/// The wire form of a completed tool invocation (revision-2 `history.tool_usage`). Only
/// identities, the outcome and the redacted error code are recorded: arguments and results stay
/// in their restricted artifacts.
fn tool_invocation(redactor: &Redactor, v: StoredInvocation) -> Result<ToolInvocation, Failure> {
    let outcome = serde_json::from_value::<ToolInvocationOutcome>(json!(v.outcome))
        .map_err(|_| stored("tool outcome"))?;
    Ok(ToolInvocation {
        sequence: rev(v.committed_sequence),
        call_id: v.call_entry_id,
        session_id: v.session_id,
        tool_name: label(&v.capability_id, "tool"),
        outcome,
        duration_ms: Some(counter(v.responded_at_ms - v.called_at_ms)),
        error_code: v
            .error_code
            .map(|code| label(&redactor.redact_text(&code), "redacted")),
        operation_id: v.operation_id,
        actor_id: v.actor,
        occurred_at_ms: counter(v.responded_at_ms),
    })
}

impl QueryHandler for HistoryService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::History(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let revision = query.schema_revision();
        let unsupported = || {
            Failure::from(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "query",
                None,
            ))
        };
        let root_scope = |root: &RootId| {
            snapshot
                .sessions()
                .work_root(root)
                .ok()
                .flatten()
                .map_or(1, |w| w.scope_revision)
        };
        let answered: Result<(OperationResult, i64), Failure> = match input {
            ops::Input::Events(q) => match &q.query {
                Query::Events {
                    root_id,
                    project_id,
                    newest_first,
                    after,
                    limit,
                } => {
                    // Admission guarantees exactly one scope; a project only from revision 2.
                    let scope = match (root_id, project_id) {
                        (Some(r), None) => Ok(EventScope::Root(r)),
                        (None, Some(p)) => Ok(EventScope::Project(p)),
                        _ => Err(unsupported()),
                    };
                    scope.and_then(|s| {
                        self.events(
                            snapshot,
                            ctx,
                            s,
                            after.as_deref(),
                            *limit,
                            newest_first.unwrap_or(false),
                            revision,
                        )
                    })
                }
                _ => Err(unsupported()),
            },
            ops::Input::OperationStatus(q) => match &q.query {
                Query::Operation { operation_id } => self
                    .operation_status(snapshot, ctx, operation_id)
                    .map(|r| (r, 1)),
                _ => Err(unsupported()),
            },
            ops::Input::CommandReceipt(q) => match &q.query {
                Query::Receipt { request_key } => self
                    .command_receipt(snapshot, ctx, request_key, revision)
                    .map(|r| (r, 1)),
                _ => Err(unsupported()),
            },
            ops::Input::WorkLineage(i) => self
                .work_lineage(snapshot, ctx, i, revision)
                .map(|r| (r, root_scope(&i.root_id))),
            ops::Input::GitLinks(i) => Self::links_page(
                snapshot,
                ctx,
                "history.git_links",
                &i.root_id,
                &["git", "pull_request"],
                &i.page,
                revision,
                |root, link| git_link(snapshot, root, link, revision),
            )
            .map(|value| {
                (
                    OperationResult::HistoryGitLinks { value },
                    root_scope(&i.root_id),
                )
            }),
            ops::Input::EvidenceLinks(i) => Self::links_page(
                snapshot,
                ctx,
                "history.evidence_links",
                &i.root_id,
                &["evidence"],
                &i.page,
                revision,
                evidence_link,
            )
            .map(|value| {
                (
                    OperationResult::HistoryEvidenceLinks { value },
                    root_scope(&i.root_id),
                )
            }),
            ops::Input::ToolUsage(i) => self.tool_usage(snapshot, ctx, i, revision),
        };
        let (result, scope_revision) =
            answered.map_err(|f| query_fault(f, "history", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope_revision)),
            result,
        })
    }
}

/// One fact of a `history.tool_usage` page, with its committed sequence.
enum Usage {
    Inference(i64, UsageItem),
    Tool(ToolInvocation),
}

/// Fault target kind of a workspace usage read outside the caller's authorized scope.
pub const WORKSPACE_SCOPE: &str = "workspace_scope";

impl HistoryService {
    /// The scope of a `history.tool_usage` page, its cursor binding and scope revision.
    /// Revision 1 names a root; revision 2 also a project (visible to the invocation, like the
    /// project change feed) or the workspace itself (its Home sessions), which only a workspace
    /// invocation of a principal whose authorized scope is the workspace reads.
    fn usage_scope(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ToolUsageInput,
        revision: SchemaRevision,
    ) -> Result<(HistoryScope, CursorBinding, i64), Failure> {
        let read = snapshot.sessions();
        let op = "history.tool_usage";
        match (&input.root_id, &input.project_id, input.workspace) {
            (Some(root_id), None, None) => {
                let root = visible_root(read, ctx, root_id)?;
                Ok((
                    HistoryScope::Root(root.id.clone()),
                    Self::binding(ctx, op, &root, revision),
                    root.scope_revision,
                ))
            }
            (None, Some(project), None) => {
                let scope_revision = match read.project_scope_revision(project)? {
                    Some(r) if project_visible(ctx, Some(project)) => r,
                    _ => return Err(not_found("project", &project.0).into()),
                };
                Ok((
                    HistoryScope::Project(project.clone()),
                    Self::bind(
                        ctx,
                        op,
                        &["project", project.0.as_str()],
                        scope_revision,
                        revision,
                    ),
                    scope_revision,
                ))
            }
            (None, None, Some(true)) => {
                let authorized = crate::services::knowledge::reach(snapshot.authority(), ctx)?;
                if ctx.project_id().is_some()
                    || authorized != crate::authority::reach::Reach::Workspace
                {
                    return Err(Refusal::new(
                        DomainErrorCode::PermissionDenied,
                        WORKSPACE_SCOPE,
                        None,
                    )
                    .into());
                }
                Ok((
                    HistoryScope::Workspace,
                    Self::bind(ctx, op, &["workspace"], 1, revision),
                    1,
                ))
            }
            // Admission admits exactly one scope.
            _ => Err(Refusal::new(DomainErrorCode::InvalidArgument, "scope", None).into()),
        }
    }

    fn tool_usage(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ToolUsageInput,
        revision: SchemaRevision,
    ) -> Result<(OperationResult, i64), Failure> {
        let read = snapshot.sessions();
        let (scope, binding, scope_revision) = Self::usage_scope(snapshot, ctx, input, revision)?;
        let inference = |u: crate::ports::storage::StoredUsage| {
            let sequence = u.committed_sequence;
            serde_json::from_value::<UsageItem>(u.item)
                .map(|item| Usage::Inference(sequence, item))
                .map_err(|_| stored("usage"))
        };
        let redactor = Redactor::new();
        let paged = Self::page(
            &binding,
            input.page.cursor.as_ref().map(|c| c.0.as_str()),
            input.page.limit.get() as u16,
            |after, n| match &scope {
                // Revision 1: the root's inference usage observations only.
                HistoryScope::Root(root) if revision.0 < 2 => read
                    .usage(root, after, n)?
                    .into_iter()
                    .map(|(k, u)| Ok((k, inference(u)?)))
                    .collect(),
                scope => read
                    .usage_facts(scope, after, n)?
                    .into_iter()
                    .map(|f| match f {
                        UsageFact::Inference { key, usage } => Ok((key, inference(usage)?)),
                        UsageFact::Tool { key, invocation } => {
                            Ok((key, Usage::Tool(tool_invocation(&redactor, invocation)?)))
                        }
                    })
                    .collect(),
            },
        )?;
        let (mut items, mut sequences, mut tools) = (Vec::new(), Vec::new(), Vec::new());
        for fact in paged.items {
            match fact {
                Usage::Inference(sequence, u) => {
                    items.push(u);
                    sequences.push(rev(sequence));
                }
                Usage::Tool(t) => tools.push(t),
            }
        }
        Ok((
            OperationResult::HistoryToolUsage {
                value: UsagePage {
                    items: BoundedList(items),
                    has_more: paged.next.is_some(),
                    next_cursor: paged.next.map(BoundedText),
                    checkpoint: Some(BoundedText(paged.checkpoint)),
                    item_sequences: Some(BoundedList(sequences)),
                    tool_invocations: Some(BoundedList(tools)),
                },
            },
            scope_revision,
        ))
    }
}
