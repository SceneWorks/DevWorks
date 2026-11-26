//! Typed requests for the Documents tab. Every mutation uses the shared document gateway.
use crate::{
    backend::Backend,
    documents_screen::{Change, ImportPlan},
    frame::{self, Negotiate},
};
use devworks_core::{
    operations::{OperationInput as In, documents as ops, indexing},
    protocol::{
        OperationId, ProjectId,
        data::{
            ArtifactCommand, ArtifactCommandRequest, ArtifactQuery, ArtifactQueryRequest, Citation,
            DataScope, DocumentVersions, IndexCommand, IndexCommandRequest, QueryLimits,
            RetrievalInput, RetrievalMode, RetrievalRequest, RetrievalSource,
        },
        provider::ArtifactRef,
        records::DocumentDetail,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DocumentSlot {
    List(u64),
    Read { generation: u64, more: bool },
    Search(u64),
    Citation(u64),
    Upload,
    Write,
    Source(u64),
    VerifyAccess(u64),
}

pub fn list(b: &dyn Backend, scope: DataScope, cursor: Option<&str>) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || {
        Some(In::Documents(ops::Input::List(ops::ListInput {
            scope,
            page: frame::page(30, cursor)?,
            retrieval_enabled: None,
        })))
    })
}

pub fn read(b: &dyn Backend, version: ArtifactRef, cursor: Option<String>) -> Vec<u8> {
    read_in(b, version, cursor, Negotiate::Latest)
}

pub fn authorize_source(b: &dyn Backend, version: ArtifactRef) -> Vec<u8> {
    read_in(
        b,
        version,
        None,
        Negotiate::pinned(10, "Exact source authority only"),
    )
}

fn read_in(
    b: &dyn Backend,
    version: ArtifactRef,
    cursor: Option<String>,
    revision: Negotiate,
) -> Vec<u8> {
    frame::with(b, revision, || {
        Some(In::Documents(ops::Input::Read(ArtifactQueryRequest {
            protocol: frame::protocol(),
            query_id: frame::query_id(b)?,
            query: ArtifactQuery::Read {
                artifact_id: version.artifact_id,
                version_id: version.version_id,
            },
            limit: 20,
            cursor,
        })))
    })
}

pub fn import(
    b: &dyn Backend,
    plan: ImportPlan,
    source: ArtifactRef,
    save_frame: &[u8],
) -> Vec<u8> {
    // Keep this receipt identical if the upload journal is recovered after import committed.
    let digest = devworks_core::fingerprint::sha256_hex(save_frame);
    frame::with_request_key(
        b,
        Negotiate::pinned(10, "Stable import replay after upload"),
        &format!("document-import-{digest}"),
        || {
            let (replace_document_id, expected_revision) = plan.replacement.unzip();
            Some(In::Documents(ops::Input::Import(ops::ImportInput {
                scope: plan.scope,
                title: frame::text(&plan.title)?,
                source,
                replace_document_id,
                expected_revision,
            })))
        },
    )
}

pub struct Search {
    pub scope: DataScope,
    pub versions: DocumentVersions,
    pub text: String,
    pub mode: RetrievalMode,
    pub fallback: bool,
}
impl Search {
    pub fn frame(self, b: &dyn Backend) -> Vec<u8> {
        frame::with(b, Negotiate::Latest, || {
            Some(In::Documents(ops::Input::Search(RetrievalRequest {
                protocol: frame::protocol(),
                query_id: frame::query_id(b)?,
                scope: self.scope,
                sources: vec![RetrievalSource::Document],
                versions: self.versions,
                input: RetrievalInput::Text { text: self.text },
                mode: self.mode,
                lexical_fallback: self.fallback,
                limits: QueryLimits {
                    max_results: 40,
                    max_visited: 20000,
                    deadline_ms: 30000,
                },
            })))
        })
    }
}

pub fn follow(b: &dyn Backend, citation: Citation) -> Vec<u8> {
    frame::build(
        b,
        Negotiate::Latest,
        Some(In::Documents(ops::Input::ResolveCitation(
            ops::ResolveCitationInput { citation },
        ))),
    )
}

pub fn operation(change: Change) -> &'static str {
    match change {
        Change::Index | Change::Reextract => "documents.index",
        Change::Retry => "documents.retry",
        Change::Cancel => "indexing.cancel",
        Change::Share => "documents.share",
        Change::Move => "documents.move",
        Change::Disable => "documents.disable_retrieval",
        Change::Remove => "documents.remove_index",
    }
}

pub fn change(
    b: &dyn Backend,
    change: Change,
    detail: DocumentDetail,
    version: ArtifactRef,
    target: Option<ProjectId>,
) -> Vec<u8> {
    frame::with(b, Negotiate::Latest, || match change {
        Change::Index | Change::Reextract => {
            Some(In::Documents(ops::Input::Index(ops::IndexInput {
                document_id: detail.id,
                version_id: version.version_id,
                expected_revision: detail.revision,
                reextract: matches!(change, Change::Reextract).then_some(true),
            })))
        }
        Change::Retry => Some(In::Documents(ops::Input::Retry(ops::RetryInput {
            document_id: detail.id,
            version_id: version.version_id,
            expected_revision: detail.revision,
        }))),
        Change::Cancel => Some(In::Indexing(indexing::Input::Cancel(IndexCommandRequest {
            protocol: frame::protocol(),
            request_key: frame::request_key(b)?,
            command: IndexCommand::Cancel {
                operation_id: OperationId(detail.index_activity?.run_id),
            },
        }))),
        _ => {
            let command = match change {
                Change::Share => ArtifactCommand::Share {
                    artifact_id: detail.id,
                    target_project_id: target?,
                    expected_scope_revision: detail.scope_revision,
                },
                Change::Move => ArtifactCommand::Move {
                    artifact_id: detail.id,
                    target_project_id: target?,
                    expected_scope_revision: detail.scope_revision,
                },
                Change::Disable => ArtifactCommand::DisableRetrieval {
                    artifact_id: detail.id,
                    expected_revision: detail.revision,
                },
                Change::Remove => ArtifactCommand::RemoveIndex {
                    artifact_id: detail.id,
                    expected_revision: detail.revision,
                },
                _ => unreachable!("document management action"),
            };
            let request = ArtifactCommandRequest {
                protocol: frame::protocol(),
                request_key: frame::request_key(b)?,
                command,
            };
            Some(In::Documents(match change {
                Change::Share => ops::Input::Share(request),
                Change::Move => ops::Input::Move(request),
                Change::Disable => ops::Input::DisableRetrieval(request),
                Change::Remove => ops::Input::RemoveIndex(request),
                _ => unreachable!("document management action"),
            }))
        }
    })
}
