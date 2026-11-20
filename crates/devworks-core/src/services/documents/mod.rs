//! Exact-version document custody. The artifact repository owns scope, bytes and retention.
use crate::{
    context::InvocationContext,
    gateway::{AdmittedCommand, AdmittedQuery},
    operations::{OperationInput, documents as ops},
    ports::{
        CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
        storage::{
            ArtifactRead, ArtifactScope, ListPosition, ReadSnapshot, StorageError, StoredArtifact,
            StoredVersion, WriteTransaction,
        },
    },
    protocol::{
        CorrelationId, DomainErrorCode, DomainFault, Id,
        data::{ArtifactCommand, ArtifactQuery, Coverage, CoverageState, DataScope},
        records::{
            DocumentDetail, DocumentLibraryPage, DocumentListResult, DocumentRecord,
            DocumentResult, ItemState,
        },
        results::{DomainDecision, OperationResult},
        support::{BoundedList, BoundedText},
    },
    services::{
        artifacts::{
            ArtifactService, Viewer,
            documents::{
                active_project, document, document_for, document_list, document_scope,
                import_document, move_document,
            },
            version_ref,
        },
        cursor::{self, CursorBinding},
        kernel::{Failure, Refusal, counter, decided, rev},
    },
};
use std::sync::Arc;

pub const LIBRARY_OPERATIONS: &[&str] = &[
    "documents.import",
    "documents.list",
    "documents.read",
    "documents.share",
    "documents.move",
];
const DETAILS_REVISION: u32 = 10;
const LISTED_VERSIONS: u16 = 10;

/// The artifact-backed document library. Its handlers can be tested through the gateway before
/// the indexing and retrieval owners make the complete document feature reachable.
pub struct DocumentLibrary {
    artifacts: Arc<ArtifactService>,
    ids: Arc<dyn IdSource>,
}

impl DocumentLibrary {
    pub fn new(artifacts: Arc<ArtifactService>, ids: Arc<dyn IdSource>) -> Self {
        Self { artifacts, ids }
    }

    fn change(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        let OperationInput::Documents(input) = command.input() else {
            return Err(invalid());
        };
        let details = command.schema_revision().0 >= DETAILS_REVISION;
        let result = match input {
            ops::Input::Import(input) => {
                let scope = document_scope(tx.authority(), ctx, &input.scope)?;
                if let Some(project) = scope.project() {
                    active_project(tx.projects(), project)?;
                }
                let replacement = match (&input.replace_document_id, &input.expected_revision) {
                    (Some(id), Some(revision)) => Some((id, revision)),
                    (None, None) => None,
                    _ => return Err(invalid()),
                };
                let artifact =
                    import_document(tx, ctx, &scope, &input.source, &input.title.0, replacement)?;
                let value = document_result(tx.artifacts(), &artifact, details)?;
                (OperationResult::DocumentsImport { value }, true)
            }
            ops::Input::Share(request) => {
                let ArtifactCommand::Share {
                    artifact_id,
                    target_project_id,
                    expected_scope_revision,
                } = &request.command
                else {
                    return Err(invalid());
                };
                let artifact = checked_document(tx, ctx, artifact_id)?;
                active_project(tx.projects(), target_project_id)?;
                let result = self.artifacts.share(
                    tx,
                    ctx,
                    &artifact.id,
                    target_project_id,
                    expected_scope_revision,
                )?;
                let artifact = checked_document(tx, ctx, artifact_id)?;
                let value = document_result(tx.artifacts(), &artifact, details)?;
                (OperationResult::DocumentsShare { value }, result.changed)
            }
            ops::Input::Move(request) => {
                let ArtifactCommand::Move {
                    artifact_id,
                    target_project_id,
                    expected_scope_revision,
                } = &request.command
                else {
                    return Err(invalid());
                };
                let artifact = checked_document(tx, ctx, artifact_id)?;
                let (artifact, changed) = move_document(
                    tx,
                    ctx,
                    artifact,
                    target_project_id,
                    expected_scope_revision,
                )?;
                let value = document_result(tx.artifacts(), &artifact, details)?;
                (OperationResult::DocumentsMove { value }, changed)
            }
            _ => return Err(invalid()),
        };
        Ok(if result.1 {
            DomainDecision::Applied { result: result.0 }
        } else {
            DomainDecision::NoChange { result: result.0 }
        })
    }

    fn answer(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, Failure> {
        let OperationInput::Documents(input) = query.input() else {
            return Err(invalid());
        };
        let details = query.schema_revision().0 >= DETAILS_REVISION;
        let (result, scope_revision) = match input {
            ops::Input::List(input) => {
                let scope = document_scope(snapshot.authority(), ctx, &input.scope)?;
                if let Some(project) = scope.project() {
                    active_project(snapshot.projects(), project)?;
                }
                let scope_key = scope
                    .project()
                    .map_or("workspace", |project| project.0.as_str());
                let sequence = snapshot
                    .stamp()
                    .read_stamp(ctx.policy_revision().clone(), rev(1))
                    .sequence
                    .get()
                    .max(1);
                let binding = CursorBinding::new(
                    ctx,
                    "documents.list",
                    &[
                        scope_key,
                        &format!("{:?}", input.retrieval_enabled),
                        ctx.policy_revision().as_str(),
                    ],
                    sequence,
                );
                let fetch = |after: Option<&str>, limit| -> Result<_, Failure> {
                    let after = after.map(parse_position).transpose()?;
                    if !details && input.retrieval_enabled == Some(false) {
                        return Ok(Vec::new());
                    }
                    let current = if details {
                        input.retrieval_enabled
                    } else {
                        Some(true)
                    };
                    Ok(document_list(
                        snapshot,
                        ctx,
                        &scope,
                        current,
                        after.as_ref(),
                        limit,
                    )?)
                };
                let page = cursor::page(&binding, &input.page, fetch, position)?;
                let value = if details {
                    let page = page.try_into_page(|artifact| {
                        detail(
                            snapshot.artifacts(),
                            &artifact,
                            None,
                            None,
                            None,
                            LISTED_VERSIONS,
                        )
                    })?;
                    DocumentListResult::Current(DocumentLibraryPage {
                        items: page.items,
                        next_cursor: page.next_cursor,
                        has_more: page.has_more,
                        snapshot_revision: rev(sequence),
                    })
                } else {
                    DocumentListResult::Legacy(page.try_into_page(|artifact| {
                        legacy(detail(
                            snapshot.artifacts(),
                            &artifact,
                            None,
                            None,
                            None,
                            LISTED_VERSIONS,
                        )?)
                    })?)
                };
                (OperationResult::DocumentsList { value }, sequence)
            }
            ops::Input::Read(request) => {
                let ArtifactQuery::Read {
                    artifact_id,
                    version_id,
                } = &request.query
                else {
                    return Err(invalid());
                };
                let artifact =
                    document(snapshot.artifacts(), snapshot.authority(), ctx, artifact_id)?;
                let selected = snapshot
                    .artifacts()
                    .version(version_id)?
                    .filter(|version| version.artifact_id == artifact.id)
                    .ok_or_else(|| unavailable(version_id))?;
                let value = if details {
                    let binding = CursorBinding::new(
                        ctx,
                        "documents.read",
                        &[
                            artifact.id.as_str(),
                            selected.id.as_str(),
                            &artifact.revision.to_string(),
                            ctx.policy_revision().as_str(),
                        ],
                        artifact.scope_revision,
                    );
                    let after = request
                        .cursor
                        .as_deref()
                        .map(|key| {
                            binding
                                .decode(key)?
                                .parse::<i64>()
                                .map_err(|_| StorageError::CursorExpired)
                        })
                        .transpose()?;
                    DocumentResult::Current(detail(
                        snapshot.artifacts(),
                        &artifact,
                        Some(&selected),
                        Some(&binding),
                        after,
                        request.limit,
                    )?)
                } else {
                    if !selected.live() || request.cursor.is_some() {
                        return Err(unavailable(version_id).into());
                    }
                    DocumentResult::Legacy(legacy(detail(
                        snapshot.artifacts(),
                        &artifact,
                        None,
                        None,
                        None,
                        500,
                    )?)?)
                };
                (
                    OperationResult::DocumentsRead { value },
                    artifact.scope_revision,
                )
            }
            _ => return Err(invalid()),
        };
        Ok(QueryAnswer {
            result,
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(scope_revision)),
        })
    }
}

impl CommandHandler for DocumentLibrary {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let decision = self.change(ctx, command, tx);
        decided(tx, decision)
    }
}

impl QueryHandler for DocumentLibrary {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        self.answer(ctx, query, snapshot).map_err(|error| {
            error.query_fault(
                "document",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })
    }
}

fn checked_document(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<StoredArtifact, Failure> {
    // The writer's repositories share one transaction, so check the scope before reading the
    // artifact with the same authority check used by the query path.
    let viewer = Viewer::resolve(tx.authority(), ctx)?;
    document_for(tx.artifacts(), &viewer, id)
}

fn document_result(
    read: &dyn ArtifactRead,
    artifact: &StoredArtifact,
    current: bool,
) -> Result<DocumentResult, Failure> {
    let view = detail(read, artifact, None, None, None, LISTED_VERSIONS)?;
    Ok(if current {
        DocumentResult::Current(view)
    } else {
        DocumentResult::Legacy(legacy(view)?)
    })
}

fn detail(
    read: &dyn ArtifactRead,
    artifact: &StoredArtifact,
    selected: Option<&StoredVersion>,
    binding: Option<&CursorBinding>,
    after: Option<i64>,
    limit: u16,
) -> Result<DocumentDetail, Failure> {
    let count = read.retained_version_count(&artifact.id)?;
    let (current, deleted) = match &artifact.current_version_id {
        Some(id) => (
            Some(
                read.version(id)?
                    .filter(|version| version.artifact_id == artifact.id && version.live())
                    .ok_or_else(|| unavailable(id))?,
            ),
            None,
        ),
        None => (
            None,
            read.version_history(&artifact.id, None, false, 1)?
                .pop()
                .filter(|version| !version.live()),
        ),
    };
    let mut history = read.version_history(&artifact.id, after, true, limit + 1)?;
    let more = history.len() > usize::from(limit);
    history.truncate(usize::from(limit));
    let next_version_cursor = binding.filter(|_| more).and_then(|binding| {
        history
            .last()
            .map(|version| binding.cursor(&version.version_no.to_string()))
    });
    let scope = match &artifact.scope {
        ArtifactScope::Workspace => DataScope::Workspace,
        ArtifactScope::Project(project_id) => DataScope::Project {
            project_id: project_id.clone(),
        },
    };
    let missing = current.is_none();
    let selected_deleted = selected.filter(|version| !version.live());
    Ok(DocumentDetail {
        id: artifact.id.clone(),
        revision: rev(artifact.revision),
        scope_revision: rev(artifact.scope_revision),
        reference_revision: rev(artifact.reference_revision),
        scope,
        title: BoundedText(artifact.title.clone()),
        current_version: current.as_ref().map(version_ref),
        selected_version: selected.filter(|version| version.live()).map(version_ref),
        deleted_selected_version_id: selected_deleted.map(|version| version.id.clone()),
        deleted_selected_at_ms: selected_deleted
            .and_then(|version| version.deleted_at_ms)
            .map(counter),
        deleted_current_version_id: deleted.as_ref().map(|version| version.id.clone()),
        deleted_current_at_ms: deleted
            .and_then(|version| version.deleted_at_ms)
            .map(counter),
        retained_versions: BoundedList(history.iter().map(version_ref).collect()),
        retained_version_count: counter(count),
        next_version_cursor,
        retrieval_enabled: !missing,
        extraction_state: ItemState::Unavailable,
        coverage: Coverage {
            state: CoverageState::Unavailable,
            gaps: vec![
                Id::new(if missing {
                    "CurrentVersionDeleted"
                } else {
                    "ExtractionPending"
                })
                .expect("literal id"),
            ],
            more_gaps: false,
        },
    })
}

fn legacy(detail: DocumentDetail) -> Result<DocumentRecord, Failure> {
    Ok(DocumentRecord {
        current_version: detail
            .current_version
            .ok_or_else(|| unavailable(&detail.id))?,
        id: detail.id,
        revision: detail.revision,
        scope: detail.scope,
        title: detail.title,
        retained_versions: detail.retained_versions,
        retrieval_enabled: detail.retrieval_enabled,
        extraction_state: detail.extraction_state,
        coverage: detail.coverage,
    })
}

fn position(artifact: &StoredArtifact) -> String {
    format!("{},{}", artifact.created_at_ms, artifact.id.as_str())
}

fn parse_position(value: &str) -> Result<ListPosition, StorageError> {
    let (at, id) = value.split_once(',').ok_or(StorageError::CursorExpired)?;
    Ok(ListPosition {
        created_at_ms: at.parse().map_err(|_| StorageError::CursorExpired)?,
        id: Id::new(id).map_err(|_| StorageError::CursorExpired)?,
    })
}

fn invalid() -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, "document_request", None).into()
}

fn unavailable(id: &Id) -> Refusal {
    Refusal::new(
        DomainErrorCode::ViewUnavailable,
        "document_version",
        Some(id.clone()),
    )
}
