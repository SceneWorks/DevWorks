//! Shared UI/tool workflow operations. Editable labels and order never change the immutable
//! semantic; removing a column only reassigns within that semantic and preserves completion,
//! run holds, ownership and acceptance evidence.
use super::workflow::{self, DEFAULT_STATUSES};
use super::{R, WorkService, event, invalid, open_project, stale, visible_project};
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::operations::workflows as ops;
use crate::ports::IdKind;
use crate::ports::storage::{
    NewEvent, ReadSnapshot, StorageError, StoredStatus, StoredWork, WORKFLOW_QUEUE, WorkFilter,
    WorkPosition, WorkRead, WorkSelection, WriteTransaction,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{MutationSummary, StatusRecord};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{Counter, EventId, Id, ProjectId, RevisionDimension};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{not_found, rev};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const MAX_STATUSES: usize = 500;

/// Canonical affected set: sorted `[workId, revision]` pairs across features and cards.
/// Including revisions fences edits as well as membership changes between preview and removal.
pub fn affected_work_digest(
    read: &dyn WorkRead,
    project: &ProjectId,
    status: &Id,
) -> Result<(Digest, usize), StorageError> {
    let rows = affected_work(read, project, status)?;
    Ok((digest(&rows), rows.len()))
}
fn digest(rows: &[StoredWork]) -> Digest {
    let mut basis: Vec<_> = rows.iter().map(|w| (w.id.as_str(), w.revision)).collect();
    basis.sort_unstable();
    Digest(sha256_hex(
        &serde_json::to_vec(&basis).expect("work identifiers serialize"),
    ))
}
fn affected_work(
    read: &dyn WorkRead,
    project: &ProjectId,
    status: &Id,
) -> Result<Vec<StoredWork>, StorageError> {
    let mut out = Vec::new();
    for selection in [WorkSelection::Features, WorkSelection::Tasks] {
        let filter = WorkFilter {
            project: project.clone(),
            selection,
            status_ids: vec![status.clone()],
            repository_ids: vec![],
            text: None,
        };
        let mut after = None;
        loop {
            let page = read.works(&filter, after.as_ref(), 500)?;
            let more = page.len() == 500;
            after = page.last().map(WorkPosition::of);
            out.extend(page);
            if !more {
                break;
            }
        }
    }
    Ok(out)
}
fn statuses(read: &dyn WorkRead, project: &ProjectId) -> Result<Vec<StoredStatus>, StorageError> {
    let rows = read.statuses(project)?;
    if !rows.is_empty() {
        return Ok(rows);
    }
    Ok(DEFAULT_STATUSES
        .iter()
        .enumerate()
        .map(|(n, (key, name, semantic))| StoredStatus {
            id: workflow::default_status_id(project, key),
            project_id: project.clone(),
            name: (*name).into(),
            semantic: (*semantic).into(),
            sort_order: n as i64 + 1,
            revision: 1,
        })
        .collect())
}
fn record(read: &dyn WorkRead, status: &StoredStatus) -> Result<StatusRecord, StorageError> {
    let (digest, count) = affected_work_digest(read, &status.project_id, &status.id)?;
    Ok(StatusRecord {
        id: status.id.clone(),
        revision: rev(status.revision),
        label: BoundedText(status.name.clone()),
        category: BoundedText(status.semantic.clone()),
        rank: Counter::new(status.sort_order).expect("status rank"),
        workflow_revision: Some(rev(read.queue_revision(&status.project_id, WORKFLOW_QUEUE)?)),
        affected_work_digest: Some(digest),
        affected_work_count: Some(Counter::new(count as i64).expect("affected count")),
    })
}
fn current_status(rows: &[StoredStatus], id: &Id) -> R<StoredStatus> {
    rows.iter()
        .find(|s| &s.id == id)
        .cloned()
        .ok_or_else(|| not_found("status", id).into())
}
fn expected_workflow(read: &dyn WorkRead, project: &ProjectId, expected: i64) -> R<i64> {
    let actual = read.queue_revision(project, WORKFLOW_QUEUE)?;
    if expected != actual {
        return Err(stale(
            RevisionDimension::Workflow,
            "workflow",
            &project.0,
            expected,
            actual,
        ));
    }
    Ok(actual)
}
fn valid_label(label: &str) -> R<()> {
    if label.trim().is_empty() {
        return Err(invalid("status_label", None));
    }
    Ok(())
}
fn history(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    revision: i64,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: Some(project.clone()),
        root_id: None,
        operation_id: None,
        aggregate_kind: "workflow".into(),
        aggregate_id: project.0.as_str().into(),
        aggregate_revision: revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
fn changed(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    kind: &str,
    payload: Value,
) -> R<i64> {
    let revision = tx.work().bump_queue(project, WORKFLOW_QUEUE)?;
    tx.work()
        .bump_queue(project, crate::ports::storage::ANY_QUEUE)?;
    history(tx, ctx, project, revision, kind, payload)?;
    Ok(revision)
}
fn reorder(tx: &mut dyn WriteTransaction, rows: &[StoredStatus]) -> Result<(), StorageError> {
    for (n, s) in rows.iter().enumerate() {
        let rank = n as i64 + 1;
        if s.sort_order != rank {
            let mut next = s.clone();
            next.sort_order = rank;
            next.revision += 1;
            tx.work().update_status(&next, s.revision)?;
        }
    }
    Ok(())
}

impl WorkService {
    pub(crate) fn list_workflow(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ListInput,
    ) -> R<(Page<StatusRecord>, i64)> {
        let project = visible_project(snapshot.projects(), ctx, &i.project_id)?;
        let rows = statuses(snapshot.work(), &project.id)?;
        let records = rows
            .iter()
            .map(|s| record(snapshot.work(), s))
            .collect::<Result<Vec<_>, _>>()?;
        let basis = sha256_hex(&serde_json::to_vec(&records).expect("status records serialize"));
        let binding = CursorBinding::new(
            ctx,
            "workflows.list",
            &[project.id.0.as_str(), &basis],
            project.scope_revision,
        );
        let offset = i
            .page
            .cursor
            .as_ref()
            .map(|c| {
                binding
                    .decode(&c.0)
                    .and_then(|v| v.parse::<usize>().map_err(|_| StorageError::CursorExpired))
            })
            .transpose()?
            .unwrap_or(0);
        if offset > records.len() {
            return Err(StorageError::CursorExpired.into());
        }
        let end = offset
            .saturating_add(i.page.limit.get() as usize)
            .min(records.len());
        let more = end < records.len();
        Ok((
            Page {
                items: BoundedList(records[offset..end].to_vec()),
                next_cursor: more.then(|| BoundedText(binding.encode(&end.to_string()))),
                has_more: more,
            },
            project.scope_revision,
        ))
    }
    pub(crate) fn create_status(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::CreateStatusInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        expected_workflow(tx.work(), &i.project_id, i.expected_workflow_revision.get())?;
        valid_label(&i.label.0)?;
        if !DEFAULT_STATUSES.iter().any(|s| s.2 == i.category.0) {
            return Err(invalid("status_category", None));
        }
        let mut rows = statuses(tx.work(), &i.project_id)?;
        if rows.len() >= MAX_STATUSES {
            return Err(invalid("status_limit", None));
        }
        let index = match &i.after_status_id {
            Some(id) => {
                rows.iter()
                    .position(|s| &s.id == id)
                    .ok_or_else(|| not_found("status", id))?
                    + 1
            }
            None => rows.len(),
        };
        // Validate everything before the first write: a refusal must not seed or reorder rows.
        workflow::ensure_workflow(tx, &i.project_id)?;
        let new = StoredStatus {
            id: tx.new_id(IdKind::Link),
            project_id: i.project_id.clone(),
            name: i.label.0.clone(),
            semantic: i.category.0.clone(),
            sort_order: index as i64 + 1,
            revision: 1,
        };
        tx.work().insert_status(&new)?;
        rows.insert(index, new.clone());
        reorder(tx, &rows)?;
        changed(
            tx,
            ctx,
            &i.project_id,
            "WorkflowStatusCreated",
            json!({"statusId":new.id,"category":new.semantic,"label":new.name}),
        )?;
        let current = current_status(&statuses(tx.work(), &i.project_id)?, &new.id)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::WorkflowsCreateStatus {
                value: record(tx.work(), &current)?,
            },
        })
    }
    pub(crate) fn rename_status(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::RenameStatusInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        valid_label(&i.label.0)?;
        let before = current_status(&statuses(tx.work(), &i.project_id)?, &i.status_id)?;
        if before.revision != i.expected_revision.get() {
            return Err(stale(
                RevisionDimension::Workflow,
                "status",
                &before.id,
                i.expected_revision.get(),
                before.revision,
            ));
        }
        if before.name == i.label.0 {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::WorkflowsRenameStatus {
                    value: record(tx.work(), &before)?,
                },
            });
        }
        workflow::ensure_workflow(tx, &i.project_id)?;
        let mut after = before.clone();
        after.name = i.label.0.clone();
        after.revision += 1;
        tx.work().update_status(&after, before.revision)?;
        changed(
            tx,
            ctx,
            &i.project_id,
            "WorkflowStatusRenamed",
            json!({"statusId":after.id,"before":before.name,"after":after.name}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::WorkflowsRenameStatus {
                value: record(tx.work(), &after)?,
            },
        })
    }
    pub(crate) fn order_statuses(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::OrderInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        expected_workflow(tx.work(), &i.project_id, i.expected_workflow_revision.get())?;
        let before = statuses(tx.work(), &i.project_id)?;
        let actual: BTreeSet<_> = before.iter().map(|s| s.id.as_str()).collect();
        let requested: BTreeSet<_> = i.ordered_status_ids.0.iter().map(Id::as_str).collect();
        if actual != requested || requested.len() != i.ordered_status_ids.0.len() {
            return Err(invalid("status_order", None));
        }
        let rows = i
            .ordered_status_ids
            .0
            .iter()
            .map(|id| current_status(&before, id))
            .collect::<R<Vec<_>>>()?;
        let same = before.iter().map(|s| &s.id).eq(rows.iter().map(|s| &s.id));
        if !same {
            workflow::ensure_workflow(tx, &i.project_id)?;
            reorder(tx, &rows)?;
            changed(
                tx,
                ctx,
                &i.project_id,
                "WorkflowStatusesOrdered",
                json!({"statusIds":i.ordered_status_ids.0}),
            )?;
        }
        let page = Page {
            items: BoundedList(
                statuses(tx.work(), &i.project_id)?
                    .iter()
                    .map(|s| record(tx.work(), s))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            next_cursor: None,
            has_more: false,
        };
        let result = OperationResult::WorkflowsOrder { value: page };
        Ok(if same {
            DomainDecision::NoChange { result }
        } else {
            DomainDecision::Applied { result }
        })
    }
    pub(crate) fn reassign_and_remove_status(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ReassignAndRemoveInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        expected_workflow(tx.work(), &i.project_id, i.expected_workflow_revision.get())?;
        let statuses = statuses(tx.work(), &i.project_id)?;
        let before = current_status(&statuses, &i.remove_status_id)?;
        let destination = current_status(&statuses, &i.destination_status_id)?;
        if before.id == destination.id || before.semantic != destination.semantic {
            return Err(invalid("status_destination", Some(&destination.id)));
        }
        let affected = affected_work(tx.work(), &i.project_id, &before.id)?;
        if digest(&affected) != i.expected_affected_work_digest {
            return Err(super::conflict("affected_work", &before.id));
        }
        for old in &affected {
            super::allocation::invalidate_work(tx, ctx, &old.id)?;
            let mut new = old.clone();
            new.status_id = Some(destination.id.clone());
            new.revision += 1;
            new.updated_at_ms = tx.now_ms();
            new.updated_by = Some(ctx.principal().clone());
            // Same semantic: never synthesize completion, erase acceptance or touch run holds.
            tx.work().update_work(&new, old.revision)?;
            event(
                tx,
                ctx,
                &new,
                "WorkStatusReassigned",
                json!({"fromStatusId":before.id,"toStatusId":destination.id,"semantic":destination.semantic}),
            )?;
        }
        let queues: BTreeSet<_> = affected.iter().map(StoredWork::queue).collect();
        for queue in queues {
            tx.work().bump_queue(&i.project_id, &queue)?;
        }
        tx.work()
            .delete_status(&i.project_id, &before.id, before.revision)?;
        let revision = changed(
            tx,
            ctx,
            &i.project_id,
            "WorkflowStatusRemoved",
            json!({"statusId":before.id,"destinationStatusId":destination.id,"affectedWorkCount":affected.len(),"affectedWorkDigest":i.expected_affected_work_digest}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::WorkflowsReassignAndRemove {
                value: MutationSummary {
                    id: before.id,
                    revision: rev(revision),
                    changed: true,
                },
            },
        })
    }
}
