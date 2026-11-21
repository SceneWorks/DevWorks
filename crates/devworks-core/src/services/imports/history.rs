//! Read inert work history and resolve exact source locations against a current native index view.
use super::publish::match_repository;
use super::service::invalid;
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::imports::HistoryInput;
use crate::ports::storage::{ACTIVE, ReadSnapshot, StorageError, UNIT_REPLACE};
use crate::protocol::records::{ImportedCodeReference, ImportedWorkEvidence};
use crate::protocol::support::{BoundedList, BoundedText, DynamicJson, Page, RelativePath};
use crate::protocol::{DomainErrorCode, Id, ProjectId, RootId};
use crate::services::cursor::CursorBinding;
use crate::services::kernel::{Failure, counter, visible_root};
use serde_json::Value;

const KINDS: [&str; 2] = ["WorkSnapshotRecordImported", "ImportedHistoricalEvidence"];

pub(super) fn history(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    input: &HistoryInput,
) -> Result<Page<ImportedWorkEvidence>, Failure> {
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
    let root = visible_root(read.sessions(), ctx, &RootId(input.work_id.clone()))?;
    if !read
        .projects()
        .project(&root.project_id)?
        .is_some_and(|project| project.lifecycle == ACTIVE)
    {
        return Err(
            crate::services::kernel::not_found("work_import_history", &input.work_id).into(),
        );
    }
    let cursor = CursorBinding::new(
        ctx,
        "imports.history",
        &[input.work_id.as_str()],
        root.scope_revision,
    );
    let after = cursor
        .resume(&input.page)?
        .map(|pos| pos.parse::<i64>())
        .transpose()
        .map_err(|_| StorageError::CursorExpired)?
        .unwrap_or(0);
    let limit = input.page.limit.get().min(50) as u16;
    let rows = read
        .sessions()
        .root_events(&root.id, &KINDS, after, limit + 1)?;
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
            current_reference(read, &root.project_id, evidence)?
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
    Ok(Page {
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
