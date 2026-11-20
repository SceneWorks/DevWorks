//! Scope and exact source validation precede lexical ranking. A citation always opens its
//! retained extraction revision. Current search and historical search use published indexes.
use super::{indexing::same_source, version_ref};
use crate::{
    context::InvocationContext,
    fingerprint::sha256_hex,
    gateway::AdmittedQuery,
    operations::{OperationInput, documents as ops},
    ports::{
        QueryAnswer,
        storage::{
            ArtifactScope, DocumentLexicalQuery, ReadSnapshot, StorageError, StoredArtifact,
        },
    },
    protocol::{
        DomainErrorCode, Id, ReadStamp, Revision,
        data::{
            Citation, CitationSource, DocumentVersions, Passage, QueryCompletion, RetrievalInput,
            RetrievalMode, RetrievalPage, RetrievalRequest, RetrievalSource,
        },
        provider::ArtifactRef,
        records::ResolvedDocumentCitation,
        results::OperationResult,
        support::BoundedText,
    },
    services::{
        artifacts::{
            Viewer,
            documents::{active_project, document_for, document_scope},
        },
        kernel::{Failure, Refusal, rev},
        knowledge::model::words,
    },
};

pub(super) fn stamp(snapshot: &dyn ReadSnapshot, ctx: &InvocationContext) -> ReadStamp {
    let metadata = snapshot.stamp();
    metadata.read_stamp(
        ctx.policy_revision().clone(),
        rev(metadata.committed_sequence.max(1)),
    )
}

pub(super) fn search(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    request: &RetrievalRequest,
) -> Result<RetrievalPage, Failure> {
    request.validate().map_err(|_| invalid())?;
    let (scope, viewer) = audience(snapshot, ctx, request)?;
    let exact = exact_indexes(snapshot, &viewer, &scope, &request.versions)?;
    let input = lexical_input(request)?;
    let words = words(input)
        .into_iter()
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>();
    let mut result = RetrievalPage {
        protocol: request.protocol.clone(),
        query_id: request.query_id.clone(),
        stamp: stamp(snapshot, ctx),
        generation_ids: vec![],
        space: None,
        effective_mode: RetrievalMode::Lexical,
        completion: QueryCompletion::Complete,
        passages: vec![],
    };
    if words.is_empty() {
        return Ok(result);
    }
    // A visited unit is an eligible posting checked for one distinct search word.
    let max_blocks = request.limits.max_visited / words.len() as u32;
    let batch = snapshot
        .documents()
        .document_lexical(&DocumentLexicalQuery {
            scope: &scope,
            owner: (!viewer.sees_owned(None)).then_some(ctx.principal()),
            exact_versions: exact.as_deref(),
            words: &words,
            max_blocks,
            max_results: request.limits.max_results,
        })?;
    let limit = usize::from(request.limits.max_results);
    result.completion = if batch.work_limited {
        QueryCompletion::WorkLimit
    } else if batch.candidates.len() > limit {
        QueryCompletion::ResultLimit
    } else {
        QueryCompletion::Complete
    };
    for (index, candidate) in batch.candidates.into_iter().take(limit).enumerate() {
        let passage = snapshot
            .documents()
            .document_passage(&candidate.checkpoint, &candidate.block)?
            .filter(|passage| {
                passage.citation.block_id == candidate.block
                    && passage.citation.content_digest.as_str()
                        == sha256_hex(passage.text.as_bytes())
            })
            .ok_or_else(unavailable)?;
        if !result.generation_ids.contains(&candidate.generation) {
            result.generation_ids.push(candidate.generation);
        }
        result.passages.push(Passage {
            chunk_id: candidate.block,
            citation: passage.citation,
            text: passage.text,
            rank: (index + 1) as u16,
        });
    }
    Ok(result)
}

fn lexical_input(request: &RetrievalRequest) -> Result<&str, Failure> {
    if !matches!(request.sources.as_slice(), [RetrievalSource::Document]) {
        return Err(
            Refusal::new(DomainErrorCode::UnsupportedTarget, "document_source", None).into(),
        );
    }
    if !(1..=1_000_000).contains(&request.limits.max_visited)
        || !(1..=300_000).contains(&request.limits.deadline_ms)
    {
        return Err(invalid().into());
    }
    match &request.input {
        RetrievalInput::Text { text }
            if matches!(request.mode, RetrievalMode::Lexical) || request.lexical_fallback =>
        {
            Ok(text)
        }
        _ => Err(Refusal::new(
            DomainErrorCode::UnsupportedTarget,
            "document_search_mode",
            None,
        )
        .into()),
    }
}

fn audience<'a>(
    snapshot: &dyn ReadSnapshot,
    ctx: &'a InvocationContext,
    request: &RetrievalRequest,
) -> Result<(ArtifactScope, Viewer<'a>), Failure> {
    let scope = document_scope(snapshot.authority(), ctx, &request.scope)?;
    if let Some(project) = scope.project() {
        active_project(snapshot.projects(), project)?;
    }
    Ok((scope, Viewer::resolve(snapshot.authority(), ctx)?))
}

fn exact_indexes(
    snapshot: &dyn ReadSnapshot,
    viewer: &Viewer<'_>,
    scope: &ArtifactScope,
    versions: &DocumentVersions,
) -> Result<Option<Vec<Id>>, Failure> {
    let DocumentVersions::Exact { versions } = versions else {
        return Ok(None);
    };
    let mut ids = Vec::new();
    for source in versions {
        visible_version(snapshot, viewer, Some(scope), source)?;
        published(snapshot, source)?;
        if !ids.contains(&source.version_id) {
            ids.push(source.version_id.clone());
        }
    }
    Ok(Some(ids))
}

fn visible_version(
    snapshot: &dyn ReadSnapshot,
    viewer: &Viewer<'_>,
    scope: Option<&ArtifactScope>,
    source: &ArtifactRef,
) -> Result<StoredArtifact, Failure> {
    let read = snapshot.artifacts();
    let artifact =
        document_for(read, viewer, &source.artifact_id, false).map_err(|error| match error {
            Failure::Refused(_) => unavailable().into(),
            error => error,
        })?;
    let in_scope = match scope {
        Some(ArtifactScope::Project(project)) => {
            artifact.scope.project() == Some(project)
                || read.shares(&artifact.id)?.contains(project)
        }
        Some(ArtifactScope::Workspace) => artifact.scope == ArtifactScope::Workspace,
        None => true,
    };
    if !in_scope
        || !read
            .version(&source.version_id)?
            .is_some_and(|version| version.live() && same_source(&version_ref(&version), source))
    {
        return Err(unavailable().into());
    }
    Ok(artifact)
}

fn published(snapshot: &dyn ReadSnapshot, source: &ArtifactRef) -> Result<(Id, Revision), Failure> {
    let repository = snapshot.documents();
    if !repository.document_retrieval_enabled(&source.artifact_id)? {
        return Err(unavailable().into());
    }
    repository
        .document_index(&source.version_id)?
        .and_then(|index| index.published)
        .ok_or_else(|| unavailable().into())
}

pub(super) fn resolve(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    citation: &Citation,
) -> Result<ResolvedDocumentCitation, Failure> {
    let checkpoint = retained_checkpoint(snapshot, ctx, citation)?;
    let passage = snapshot
        .documents()
        .document_passage(&checkpoint, &citation.block_id)?
        .ok_or_else(unavailable)?;
    let same_citation = matches!(
        (serde_json::to_value(&passage.citation), serde_json::to_value(citation)),
        (Ok(got), Ok(requested)) if got == requested
    );
    if !same_citation || citation.content_digest.as_str() != sha256_hex(passage.text.as_bytes()) {
        return Err(unavailable().into());
    }
    Ok(ResolvedDocumentCitation {
        citation: passage.citation,
        text: BoundedText(passage.text),
    })
}

fn retained_checkpoint(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    citation: &Citation,
) -> Result<Id, Failure> {
    let CitationSource::Managed {
        version,
        extraction_revision,
    } = &citation.source
    else {
        return Err(Refusal::new(
            DomainErrorCode::UnsupportedTarget,
            "document_citation",
            None,
        )
        .into());
    };
    let viewer = Viewer::resolve(snapshot.authority(), ctx)?;
    visible_version(snapshot, &viewer, None, version)?;
    let checkpoint = snapshot
        .documents()
        .document_checkpoint(&version.version_id, extraction_revision)?
        .filter(|value| value.source_digest == version.sha256.as_str())
        .ok_or_else(unavailable)?;
    Ok(checkpoint.id)
}

/// The gateway calls this on a fresh snapshot after the answer exists. Movement, deletion and
/// revoked shares fence the answer even when the original query snapshot could read them.
pub(super) fn check_publication(
    ctx: &InvocationContext,
    query: &AdmittedQuery,
    answer: &QueryAnswer,
    snapshot: &dyn ReadSnapshot,
) -> Result<bool, StorageError> {
    if &snapshot.stamp().store_id != ctx.store_id()
        || snapshot.stamp().restore_epoch != ctx.restore_epoch().get()
    {
        return Ok(false);
    }
    let checked = match (query.input(), &answer.result) {
        (
            OperationInput::Documents(ops::Input::Search(request)),
            OperationResult::DocumentsSearch { value },
        ) => check_result_scope(snapshot, ctx, request, value),
        (
            OperationInput::Documents(ops::Input::ResolveCitation(request)),
            OperationResult::DocumentsResolveCitation { .. },
        ) => retained_checkpoint(snapshot, ctx, &request.citation).map(|_| true),
        _ => Ok(false),
    };
    match checked {
        Ok(allowed) => Ok(allowed),
        Err(Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied)) => Ok(false),
        Err(Failure::Storage(error)) => Err(error),
    }
}

fn check_result_scope(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    request: &RetrievalRequest,
    result: &RetrievalPage,
) -> Result<bool, Failure> {
    let (scope, viewer) = audience(snapshot, ctx, request)?;
    let exact = exact_indexes(snapshot, &viewer, &scope, &request.versions)?;
    for passage in &result.passages {
        let CitationSource::Managed {
            version,
            extraction_revision,
        } = &passage.citation.source
        else {
            return Ok(false);
        };
        let artifact = visible_version(snapshot, &viewer, Some(&scope), version)?;
        let (generation, revision) = published(snapshot, version)?;
        let selected = exact.as_ref().map_or_else(
            || artifact.current_version_id.as_ref() == Some(&version.version_id),
            |ids| ids.contains(&version.version_id),
        );
        if !selected
            || &revision != extraction_revision
            || !result.generation_ids.contains(&generation)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn unavailable() -> Refusal {
    Refusal::new(DomainErrorCode::ViewUnavailable, "document_version", None)
}

fn invalid() -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, "document_search", None)
}
