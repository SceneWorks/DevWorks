//! Exact-version document custody. The artifact repository owns scope, bytes and retention.
pub mod extraction;
pub mod indexing;
pub mod pdf;
mod retrieval;

use crate::{
    context::InvocationContext,
    gateway::{AdmittedCommand, AdmittedQuery},
    operations::{OperationInput, documents as ops, indexing as index_ops},
    ports::{
        Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer,
        QueryHandler, QueryPublicationCheck,
        storage::{
            ArtifactRead, ArtifactScope, DocumentIndexStage, DocumentListFilter, DocumentRead,
            ListPosition, QueueRead, QueueState, ReadSnapshot, StorageError, StoredArtifact,
            StoredVersion, WriteTransaction,
        },
    },
    protocol::{
        CorrelationId, DomainErrorCode, DomainFault, Id,
        data::{ArtifactCommand, ArtifactQuery, Coverage, CoverageState, DataScope, IndexCommand},
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
                active_project, document_for, document_list, document_scope, import_document,
                move_document, read_document, require_workspace_authority,
            },
            version_ref,
        },
        cursor::{self, CursorBinding},
        kernel::{Failure, Refusal, counter, decided, rev},
    },
};
use indexing::{DocumentIndexer, check_revision, code, writable_document};
use std::sync::Arc;

pub const LIBRARY_OPERATIONS: &[&str] = &[
    "documents.import",
    "documents.list",
    "documents.read",
    "documents.share",
    "documents.move",
];
pub const INDEX_OPERATIONS: &[&str] = &[
    "documents.index",
    "documents.retry",
    "documents.disable_retrieval",
    "documents.remove_index",
];
pub const RETRIEVAL_OPERATIONS: &[&str] = &["documents.search", "documents.resolve_citation"];
const DETAILS_REVISION: u32 = 10;
const LISTED_VERSIONS: u16 = 10;

/// The artifact-backed document library. Its handlers can be tested through the gateway before
/// the indexing and retrieval owners make the complete document feature reachable.
pub struct DocumentLibrary {
    artifacts: Arc<ArtifactService>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    indexer: Option<Arc<DocumentIndexer>>,
}

impl DocumentLibrary {
    pub fn new(
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            artifacts,
            ids,
            clock,
            indexer: None,
        }
    }

    pub fn with_indexer(mut self, indexer: Arc<DocumentIndexer>) -> Self {
        self.indexer = Some(indexer);
        self
    }

    pub(crate) fn has_indexer(&self) -> bool {
        self.indexer.is_some()
    }

    fn change(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        if let OperationInput::Indexing(index_ops::Input::Cancel(input)) = command.input()
            && let IndexCommand::Cancel { operation_id } = &input.command
        {
            return indexing::cancel_if_document(tx, ctx, &operation_id.0)?.ok_or_else(|| {
                crate::services::kernel::not_found("index_run", &operation_id.0).into()
            });
        }
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
                let value = document_result(tx, &artifact, details)?;
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
                require_workspace_authority(tx.authority(), ctx)?;
                active_project(tx.projects(), target_project_id)?;
                let result = self.artifacts.share(
                    tx,
                    ctx,
                    &artifact.id,
                    target_project_id,
                    expected_scope_revision,
                )?;
                let artifact = checked_document(tx, ctx, artifact_id)?;
                let value = document_result(tx, &artifact, details)?;
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
                let value = document_result(tx, &artifact, details)?;
                (OperationResult::DocumentsMove { value }, changed)
            }
            ops::Input::Index(input) => {
                let indexer = self.indexer.as_ref().ok_or_else(invalid)?;
                let value = indexer.admit(
                    tx,
                    ctx,
                    command,
                    &input.document_id,
                    &input.version_id,
                    &input.expected_revision,
                )?;
                (OperationResult::DocumentsIndex { value }, true)
            }
            ops::Input::Retry(input) => {
                let indexer = self.indexer.as_ref().ok_or_else(invalid)?;
                let value = indexer.admit(
                    tx,
                    ctx,
                    command,
                    &input.document_id,
                    &input.version_id,
                    &input.expected_revision,
                )?;
                (OperationResult::DocumentsRetry { value }, true)
            }
            ops::Input::DisableRetrieval(request) | ops::Input::RemoveIndex(request) => {
                let (id, expected, disable) = match (&request.command, input) {
                    (
                        ArtifactCommand::DisableRetrieval {
                            artifact_id,
                            expected_revision,
                        },
                        ops::Input::DisableRetrieval(_),
                    ) => (artifact_id, expected_revision, true),
                    (
                        ArtifactCommand::RemoveIndex {
                            artifact_id,
                            expected_revision,
                        },
                        ops::Input::RemoveIndex(_),
                    ) => (artifact_id, expected_revision, false),
                    _ => return Err(invalid()),
                };
                let mut artifact = writable_document(tx, ctx, id)?;
                check_revision(&artifact, expected)?;
                let changed = if disable {
                    // An explicit disable fences earlier reindex requests even when a prior
                    // command already disabled the document.
                    tx.documents().set_document_retrieval(id, false)?;
                    true
                } else {
                    tx.documents().remove_document_index(id)?
                };
                if changed {
                    artifact.revision = expected.checked_next().ok_or_else(invalid)?.get();
                    tx.artifacts().update_artifact(&artifact)?;
                }
                let value = document_result(tx, &artifact, details)?;
                let result = if disable {
                    OperationResult::DocumentsDisableRetrieval { value }
                } else {
                    OperationResult::DocumentsRemoveIndex { value }
                };
                (result, changed)
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
            ops::Input::Search(request) => {
                let value = retrieval::search(snapshot, ctx, request)?;
                return Ok(QueryAnswer {
                    stamp: value.stamp.clone(),
                    result: OperationResult::DocumentsSearch { value },
                });
            }
            ops::Input::ResolveCitation(request) => {
                let value = retrieval::resolve(snapshot, ctx, &request.citation)?;
                return Ok(QueryAnswer {
                    stamp: retrieval::stamp(snapshot, ctx),
                    result: OperationResult::DocumentsResolveCitation { value },
                });
            }
            ops::Input::List(input) => {
                let scope = document_scope(snapshot.authority(), ctx, &input.scope)?;
                if let Some(project) = scope.project() {
                    active_project(snapshot.projects(), project)?;
                }
                let scope_key = match &scope {
                    ArtifactScope::Workspace => "Workspace".to_owned(),
                    ArtifactScope::Project(project) => format!("Project:{}", project.0.as_str()),
                };
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
                        &scope_key,
                        &query.schema_revision().0.to_string(),
                        &format!("{:?}", input.retrieval_enabled),
                        ctx.policy_revision().as_str(),
                    ],
                    sequence,
                );
                let fetch = |after: Option<&str>, limit| -> Result<_, Failure> {
                    let after = after.map(parse_position).transpose()?;
                    let filter = DocumentListFilter {
                        retrieval_enabled: input.retrieval_enabled,
                        current_only: !details,
                    };
                    Ok(document_list(
                        snapshot,
                        ctx,
                        &scope,
                        filter,
                        after.as_ref(),
                        limit,
                    )?)
                };
                let page = cursor::page(&binding, &input.page, fetch, position)?;
                let value = if details {
                    let page = page.try_into_page(|artifact| {
                        read_detail(snapshot, &artifact, None, None, None, LISTED_VERSIONS)
                    })?;
                    DocumentListResult::Current(DocumentLibraryPage {
                        items: page.items,
                        next_cursor: page.next_cursor,
                        has_more: page.has_more,
                        snapshot_revision: rev(sequence),
                    })
                } else {
                    DocumentListResult::Legacy(page.try_into_page(|artifact| {
                        legacy(read_detail(
                            snapshot,
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
                    read_document(snapshot.artifacts(), snapshot.authority(), ctx, artifact_id)?;
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
                    DocumentResult::Current(read_detail(
                        snapshot,
                        &artifact,
                        Some(&selected),
                        Some(&binding),
                        after,
                        request.limit,
                    )?)
                } else {
                    if request.cursor.is_some() {
                        return Err(StorageError::CursorExpired.into());
                    }
                    if !selected.live() {
                        return Err(unavailable(version_id).into());
                    }
                    DocumentResult::Legacy(legacy(read_detail(
                        snapshot,
                        &artifact,
                        Some(&selected),
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
    fn read_deadline_ms(&self, query: &AdmittedQuery) -> Option<i64> {
        let OperationInput::Documents(ops::Input::Search(request)) = query.input() else {
            return None;
        };
        Some(
            self.clock
                .now_ms()
                .get()
                .saturating_add(i64::from(request.limits.deadline_ms.clamp(1, 300_000))),
        )
    }

    fn publication_check(&self, query: &AdmittedQuery) -> Option<QueryPublicationCheck> {
        matches!(
            query.input(),
            OperationInput::Documents(ops::Input::Search(_) | ops::Input::ResolveCitation(_))
        )
        .then_some(retrieval::check_publication)
    }

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
    document_for(tx.artifacts(), &viewer, id, false)
}

fn document_result(
    tx: &mut dyn WriteTransaction,
    artifact: &StoredArtifact,
    current: bool,
) -> Result<DocumentResult, Failure> {
    let mut view = detail(tx.artifacts(), artifact, None, None, None, LISTED_VERSIONS)?;
    index_metadata(&mut view, tx.documents(), None)?;
    Ok(if current {
        DocumentResult::Current(view)
    } else {
        DocumentResult::Legacy(legacy(view)?)
    })
}

fn read_detail(
    snapshot: &dyn ReadSnapshot,
    artifact: &StoredArtifact,
    selected: Option<&StoredVersion>,
    binding: Option<&CursorBinding>,
    after: Option<i64>,
    limit: u16,
) -> Result<DocumentDetail, Failure> {
    let mut view = detail(
        snapshot.artifacts(),
        artifact,
        selected,
        binding,
        after,
        limit,
    )?;
    index_metadata(&mut view, snapshot.documents(), Some(snapshot.queue()))?;
    Ok(view)
}

fn index_metadata(
    view: &mut DocumentDetail,
    read: &dyn DocumentRead,
    queue: Option<&dyn QueueRead>,
) -> Result<(), StorageError> {
    view.retrieval_enabled = view.retrieval_enabled && read.document_retrieval_enabled(&view.id)?;
    if view.deleted_selected_version_id.is_some() {
        view.coverage.gaps = vec![code("SelectedVersionDeleted")];
        return Ok(());
    }
    let Some(version) = view
        .selected_version
        .as_ref()
        .or(view.current_version.as_ref())
    else {
        return Ok(());
    };
    let Some(index) = read.document_index(&version.version_id)? else {
        return Ok(());
    };
    view.coverage.gaps.clear();
    if index.stage == DocumentIndexStage::Removed
        && let Some(checkpoint) = read.latest_document_checkpoint(&version.version_id)?
    {
        view.coverage = checkpoint.coverage;
        view.extraction_state = match view.coverage.state {
            CoverageState::Complete => ItemState::Healthy,
            CoverageState::Partial => ItemState::Partial,
            CoverageState::Unavailable => ItemState::Unavailable,
        };
        return Ok(());
    }
    view.extraction_state = match index.stage {
        DocumentIndexStage::Ready => ItemState::Healthy,
        DocumentIndexStage::Partial => ItemState::Partial,
        DocumentIndexStage::Failed => ItemState::Failed,
        _ => ItemState::Unavailable,
    };
    if let Some((_, revision)) = &index.published
        && let Some(checkpoint) = read.document_checkpoint(&version.version_id, revision)?
    {
        view.coverage = checkpoint.coverage;
    }
    let note = match index.stage {
        DocumentIndexStage::Queued | DocumentIndexStage::Extracted => {
            let entry = queue
                .map(|queue| queue.entry(&index.latest))
                .transpose()?
                .flatten();
            Some(
                if entry
                    .as_ref()
                    .is_some_and(|entry| entry.state == QueueState::Cancelled)
                {
                    code("IndexCancelled")
                } else if entry.as_ref().is_some_and(|entry| {
                    matches!(entry.state, QueueState::Failed | QueueState::Dismissed)
                }) {
                    view.extraction_state = ItemState::Failed;
                    code("IndexFailed")
                } else if queue.is_some() && entry.is_none() {
                    code("IndexUnavailable")
                } else {
                    code("IndexPending")
                },
            )
        }
        DocumentIndexStage::Removed => Some(code("IndexRemoved")),
        _ => index.diagnostic,
    };
    if let Some(note) = note {
        view.coverage.more_gaps |= view.coverage.gaps.len() >= 128;
        view.coverage.gaps.truncate(127);
        view.coverage.gaps.push(note);
        if matches!(view.coverage.state, CoverageState::Complete) {
            view.coverage.state = CoverageState::Partial;
        }
    }
    Ok(())
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
    let mut retained_versions = detail.retained_versions;
    if let Some(selected) = detail.selected_version
        && !retained_versions
            .0
            .iter()
            .any(|version| version.version_id == selected.version_id)
    {
        if retained_versions.0.len() == 500 {
            retained_versions.0.pop();
        }
        retained_versions.0.push(selected);
    }
    Ok(DocumentRecord {
        current_version: detail
            .current_version
            .ok_or_else(|| unavailable(&detail.id))?,
        id: detail.id,
        revision: detail.revision,
        scope: detail.scope,
        title: detail.title,
        retained_versions,
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
