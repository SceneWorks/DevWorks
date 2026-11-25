//! Read inert work history and resolve exact source locations against a current native index view.
use super::publish::match_repository;
use super::service::invalid;
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::imports::HistoryInput;
use crate::ports::storage::{ACTIVE, EventFilter, ReadSnapshot, StorageError, UNIT_REPLACE};
use crate::protocol::records::{
    ImportedCodeReference, ImportedWorkEvidence, ImportedWorkEvidencePage,
};
use crate::protocol::support::{BoundedList, BoundedText, DynamicJson, RelativePath};
use crate::protocol::{DomainErrorCode, Id, ProjectId, RootId};
use crate::services::cursor::CursorBinding;
use crate::services::kernel::{Failure, counter, visible_root};
use serde_json::Value;

const KINDS: [&str; 2] = ["WorkSnapshotRecordImported", "ImportedHistoricalEvidence"];

pub(super) fn history(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &HistoryInput,
) -> Result<ImportedWorkEvidencePage, Failure> {
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.root_id().is_some()
        || ctx.session_id().is_some()
        || ctx.job_run_id().is_some()
    {
        return Err(super::service::refuse(
            DomainErrorCode::PermissionDenied,
            "work_import_history",
        ));
    }
    let root = match (&input.work_id, &input.project_id) {
        (Some(work), None) => Some(visible_root(read.sessions(), ctx, &RootId(work.clone()))?),
        (None, Some(project)) => {
            crate::services::kernel::require_project(ctx, project)?;
            None
        }
        _ => {
            return Err(super::service::refuse(
                DomainErrorCode::InvalidArgument,
                "work_import_history_scope",
            ));
        }
    };
    let project_id = root
        .as_ref()
        .map(|root| &root.project_id)
        .or(input.project_id.as_ref())
        .ok_or_else(|| invalid("import project scope"))?;
    let project = read
        .projects()
        .project(project_id)?
        .filter(|project| project.lifecycle == ACTIVE)
        .ok_or_else(|| crate::services::kernel::not_found("work_import_history", &project_id.0))?;
    let (kind, key, revision) = match &root {
        Some(root) => ("work", &root.id.0, root.scope_revision),
        None => ("project", &project.id.0, project.scope_revision),
    };
    let cursor = CursorBinding::new(ctx, "imports.history", &[kind, key.as_str()], revision);
    let after = cursor
        .resume(&input.page)?
        .map(|pos| pos.parse::<i64>())
        .transpose()
        .map_err(|_| StorageError::CursorExpired)?
        .unwrap_or(0);
    let limit = input.page.limit.get().min(50) as u16;
    let rows = if let Some(root) = &root {
        read.sessions()
            .root_events(&root.id, &KINDS, after, limit + 1)?
    } else {
        let filter = EventFilter::Aggregate {
            kind: "imported_project_work".into(),
            id: project.id.0.as_str().into(),
        };
        read.history().page(&filter, after, limit + 1)?.events
    };
    let mut output = Vec::new();
    let mut position = after;
    let mut bytes = 0;
    let count = rows.len();
    for event in rows.into_iter().take(limit as usize) {
        let payload = event.payload;
        let kind = payload["kind"]
            .as_str()
            .ok_or_else(|| invalid("imported history kind"))?;
        let source_id = parse_id(&payload, "sourceIdentity")?;
        let evidence = payload
            .get("evidence")
            .or_else(|| payload.get("source"))
            .ok_or_else(|| invalid("imported history payload"))?;
        let current_reference = if kind == "CodeReference" {
            current_reference(read, &project.id, evidence)?
        } else {
            None
        };
        let row = ImportedWorkEvidence {
            id: event.id.0,
            source_namespace: parse_id(&payload, "sourceNamespace")?,
            source_id,
            kind: BoundedText(kind.into()),
            source_created_at_ms: counter(
                payload
                    .pointer("/stamp/createdAtMs")
                    .or_else(|| payload.pointer("/source/stamp/createdAtMs"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            ),
            evidence: DynamicJson(
                serde_json::json!({"source":evidence,"stamp":payload.get("stamp")}),
            ),
            current_reference,
        };
        let length = serde_json::to_vec(&row)
            .map_err(|_| invalid("imported history row"))?
            .len();
        if bytes + length > crate::framing::MAX_FRAME_BYTES - 16_384 {
            if output.is_empty() {
                return Err(StorageError::ViewUnavailable.into());
            }
            break;
        }
        bytes += length;
        position = event.sequence;
        output.push(row);
    }
    let has_more = output.len() < count;
    Ok(ImportedWorkEvidencePage {
        items: BoundedList(output),
        has_more,
        next_cursor: has_more.then(|| cursor.cursor(&position.to_string())),
    })
}

fn parse_id(payload: &Value, key: &str) -> Result<Id, StorageError> {
    serde_json::from_value(payload[key].clone()).map_err(|_| invalid("imported history identity"))
}

fn current_reference(
    read: &dyn ReadSnapshot,
    project: &ProjectId,
    source: &Value,
) -> Result<Option<ImportedCodeReference>, StorageError> {
    let Some(commit) = source["commit"]
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        return Ok(None);
    };
    let Ok(path) = serde_json::from_value::<RelativePath>(source["path"].clone()) else {
        return Ok(None);
    };
    let Some(repository_id) =
        match_repository(read.projects(), source["remote"].as_str(), Some(project))?
    else {
        return Ok(None);
    };
    let index = read.indexing();
    let Some(published) = index.published(&repository_id)? else {
        return Ok(None);
    };
    let Some(view) = index.view(&published.view_id)?.filter(|view| {
        view.repository_id == repository_id
            && view.branch_id.is_none()
            && view.worktree_id.is_none()
    }) else {
        return Ok(None);
    };
    let Some(source) = index.generation(&view.baseline_id)?.filter(|base| {
        base.repository_id == repository_id && base.source_digest.eq_ignore_ascii_case(commit)
    }) else {
        return Ok(None);
    };
    if !index
        .unit(&source.id, &path.0)?
        .is_some_and(|unit| unit.action == UNIT_REPLACE)
    {
        return Ok(None);
    }
    Ok(Some(ImportedCodeReference {
        repository_id,
        view_id: view.view_id,
        commit: BoundedText(commit.to_owned()),
        path,
    }))
}
