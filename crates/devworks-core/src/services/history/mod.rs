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
//! against the invocation's project scope: a root outside it is `NotFound`. Cursors are
//! versioned and bound to the query, root, principal, project, store, restore epoch and the
//! root's scope revision; any change expires them (`CursorExpired`). The gateway re-checks the
//! authorization's fences on a fresh snapshot before a page is published.
//!
//! `history.events` publishes the facts the recorded `Event` contract can type for a root: the
//! `CommandDecided` decision of every command whose target was that work item (or whose durable
//! operation it names), with that real subject and its revision dimension and value after the
//! decision. A decision attached to the root only by the invoking agent's lineage has no
//! knowable aggregate the `Subject` union can express, so it appears in `history.work_lineage`
//! instead of as an event with a fabricated aggregate. A `Page<Event>` cannot carry a
//! continuation at its tail (its recorded rule ties `nextCursor` to `hasMore`); a poller resumes
//! from its last page's cursor. Lineage records have no actor, time, repository or revision
//! fields, so their summaries carry them.
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
//! `WorkController.Changes`, `WorkViewsController.ItemChanges` and the `changes` work tool map to
//! `history.events` of the item; `WorkViewsController.GitHubEvidence` to `history.git_links`. A
//! project-wide feed (`RecentChanges`, `changes` without an item) has no recorded input: every
//! history query is root-scoped. `AdminMcpHubController.Audit`, the `AdminReportsController`
//! reports and the `McpToolInvocationRecordedConsumer` map to `history.tool_usage`, whose
//! recorded result (`UsageItem`) is inference usage: the attributed observations are appended
//! with their session batch and read per root; tool-invocation facts (tool name, success,
//! duration, error) have no field there. Price classification, budgets and report filters are
//! the usage item's (`router.usage`).
use crate::context::{InvocationContext, ReceiptIdentity};
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, history as ops};
use crate::ports::storage::{
    LedgerEntry, LineageRow, NewEvent, ReadSnapshot, SequenceKey, StorageError, StoredLink,
    WorkRoot, WriteTransaction,
};
use crate::ports::{IdKind, IdSource, QueryAnswer, QueryHandler};
use crate::protocol::data::GitOid;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    EvidenceLink, GitLink, LineageRecord, OperationRef, ReceiptLookupState, ReceiptObservation,
    UsageItem,
};
use crate::protocol::results::OperationResult;
use crate::protocol::shared::{Event, EventBody, Query, RevisionKind, Subject};
use crate::protocol::support::{BoundedList, BoundedText, Page, Text, Url};
use crate::protocol::{
    CommandId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, Protocol, RequestKey,
    RetryAdvice, RootId,
};
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

/// A lineage summary with the attribution the record has no fields for (HISTORY-02): what
/// happened, by whom, when (UTC ms) and at which committed sequence.
fn attributed(what: &str, actor: &crate::protocol::ActorId, at_ms: i64, sequence: i64) -> String {
    format!(
        "{what} - by {} at {at_ms} (sequence {sequence})",
        actor.0.as_str()
    )
}

/// The recorded subject of a decision (`payload.subject`): a work item at its work revision or
/// an operation at its operation revision.
fn decision_subject(payload: &Value) -> Result<(Subject, RevisionKind, i64), Failure> {
    let s = &payload["subject"];
    let id = s["id"]
        .as_str()
        .and_then(|i| Id::new(i).ok())
        .ok_or_else(|| stored("decision subject"))?;
    let revision = s["revision"]
        .as_i64()
        .filter(|r| *r > 0)
        .ok_or_else(|| stored("decision subject revision"))?;
    match (s["kind"].as_str(), s["dimension"].as_str()) {
        (Some("root"), Some("Work")) => {
            Ok((Subject::Root(RootId(id)), RevisionKind::Work, revision))
        }
        (Some("operation"), Some("Operation")) => Ok((
            Subject::Operation(crate::protocol::OperationId(id)),
            RevisionKind::Operation,
            revision,
        )),
        _ => Err(stored("decision subject kind")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitBody {
    worktree_id: Option<Id>,
    branch: Option<String>,
    pull_request_url: Option<String>,
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

pub struct HistoryService {
    ids: Arc<dyn IdSource>,
}

/// One bounded page of rows with their keys.
struct Paged<T> {
    items: Vec<T>,
    next: Option<String>,
}

impl HistoryService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    fn binding(ctx: &InvocationContext, op: &str, root: &WorkRoot) -> CursorBinding {
        CursorBinding::new(ctx, op, &[root.id.0.as_str()], root.scope_revision)
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
        Ok(Paged {
            items: rows.into_iter().map(|(_, t)| t).collect(),
            next,
        })
    }

    fn events(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        root_id: &RootId,
        after: Option<&str>,
        limit: u16,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, root_id)?;
        let binding = Self::binding(ctx, "history.events", &root);
        let paged = Self::page(&binding, after, limit, |after, n| {
            let from = after.map_or(0, |k| k.sequence);
            if from > snapshot.stamp().committed_sequence {
                return Err(StorageError::CursorExpired.into());
            }
            let rows = read.root_events(&root.id, &["CommandDecided"], from, n)?;
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
                    // The decision's real subject at its revision after the decision.
                    let (aggregate, kind, revision) = decision_subject(&e.payload)?;
                    Ok((
                        key,
                        Event {
                            protocol: Protocol::Draft1,
                            event_id: e.id.clone(),
                            sequence: rev(e.sequence),
                            aggregate,
                            aggregate_revision: rev(revision),
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
        Ok(OperationResult::HistoryEvents {
            value: Page {
                items: BoundedList(paged.items),
                has_more: paged.next.is_some(),
                next_cursor: paged.next.map(BoundedText),
            },
        })
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
                }
            }
        })
    }

    fn work_lineage(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::WorkLineageInput,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, &input.root_id)?;
        // Related work rows have fixed keys at the start of the order, so a hierarchy change
        // (work added, moved in or out) expires the cursor visibly instead of skipping it.
        let membership = read.work_membership(&root.id)?;
        let binding = CursorBinding::new(
            ctx,
            "history.work_lineage",
            &[root.id.0.as_str(), &membership],
            root.scope_revision,
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
            value: Page {
                items: BoundedList(paged.items),
                has_more: paged.next.is_some(),
                next_cursor: paged.next.map(BoundedText),
            },
        })
    }

    fn links_page<T>(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        op: &str,
        root_id: &RootId,
        kinds: &[&str],
        page: &crate::protocol::records::PageRequest,
        map: impl Fn(&WorkRoot, StoredLink) -> Result<T, Failure>,
    ) -> Result<Paged<T>, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, root_id)?;
        let binding = Self::binding(ctx, op, &root);
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
    }

    fn operation_status(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        operation_id: &crate::protocol::OperationId,
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
                },
            }),
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

fn git_link(root: &WorkRoot, l: StoredLink) -> Result<GitLink, Failure> {
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
    Ok(GitLink {
        repository_id: l.repository_id.ok_or_else(|| stored("git repository"))?,
        work_id: root.id.0.clone(),
        worktree_id: body.worktree_id,
        branch,
        pull_request_url: url,
        head: body.head,
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
        let unsupported = || {
            Failure::from(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "query",
                None,
            ))
        };
        let (answered, root) = match input {
            ops::Input::Events(q) => match &q.query {
                Query::Events {
                    root_id,
                    after,
                    limit,
                } => (
                    self.events(snapshot, ctx, root_id, after.as_deref(), *limit),
                    Some(root_id),
                ),
                _ => (Err(unsupported()), None),
            },
            ops::Input::OperationStatus(q) => match &q.query {
                Query::Operation { operation_id } => {
                    (self.operation_status(snapshot, ctx, operation_id), None)
                }
                _ => (Err(unsupported()), None),
            },
            ops::Input::CommandReceipt(q) => match &q.query {
                Query::Receipt { request_key } => {
                    (self.command_receipt(snapshot, ctx, request_key), None)
                }
                _ => (Err(unsupported()), None),
            },
            ops::Input::WorkLineage(i) => (self.work_lineage(snapshot, ctx, i), Some(&i.root_id)),
            ops::Input::GitLinks(i) => (
                Self::links_page(
                    snapshot,
                    ctx,
                    "history.git_links",
                    &i.root_id,
                    &["git", "pull_request"],
                    &i.page,
                    git_link,
                )
                .map(|p| OperationResult::HistoryGitLinks {
                    value: Page {
                        items: BoundedList(p.items),
                        has_more: p.next.is_some(),
                        next_cursor: p.next.map(BoundedText),
                    },
                }),
                Some(&i.root_id),
            ),
            ops::Input::EvidenceLinks(i) => (
                Self::links_page(
                    snapshot,
                    ctx,
                    "history.evidence_links",
                    &i.root_id,
                    &["evidence"],
                    &i.page,
                    evidence_link,
                )
                .map(|p| OperationResult::HistoryEvidenceLinks {
                    value: Page {
                        items: BoundedList(p.items),
                        has_more: p.next.is_some(),
                        next_cursor: p.next.map(BoundedText),
                    },
                }),
                Some(&i.root_id),
            ),
            ops::Input::ToolUsage(i) => (self.tool_usage(snapshot, ctx, i), Some(&i.root_id)),
        };
        let result = answered.map_err(|f| query_fault(f, "history", self.correlation()))?;
        let scope_revision = root
            .and_then(|r| snapshot.sessions().work_root(r).ok().flatten())
            .map_or(1, |w| w.scope_revision);
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope_revision)),
            result,
        })
    }
}

impl HistoryService {
    fn tool_usage(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ToolUsageInput,
    ) -> Result<OperationResult, Failure> {
        let read = snapshot.sessions();
        let root = visible_root(read, ctx, &input.root_id)?;
        let binding = Self::binding(ctx, "history.tool_usage", &root);
        let paged = Self::page(
            &binding,
            input.page.cursor.as_ref().map(|c| c.0.as_str()),
            input.page.limit.get() as u16,
            |after, n| {
                read.usage(&root.id, after, n)?
                    .into_iter()
                    .map(|(k, u)| {
                        let item: UsageItem =
                            serde_json::from_value(u.item).map_err(|_| stored("usage"))?;
                        Ok((k, item))
                    })
                    .collect()
            },
        )?;
        Ok(OperationResult::HistoryToolUsage {
            value: Page {
                items: BoundedList(paged.items),
                has_more: paged.next.is_some(),
                next_cursor: paged.next.map(BoundedText),
            },
        })
    }
}
