//! A projection of existing exact-version index history and metered attempts.
use super::indexing::code;
use crate::{
    authority::reach::{Reach, reach},
    context::InvocationContext,
    gateway::AdmittedQuery,
    operations::{OperationInput, documents as ops},
    ports::{
        QueryAnswer,
        storage::{DocumentIndexStage, QueueState, ReadSnapshot, StorageError},
    },
    protocol::{
        DomainErrorCode, Id,
        data::ArtifactQuery,
        records::{DocumentIndexActivity, DocumentIndexAttempt, DocumentResult},
        results::OperationResult,
        support::{BoundedList, BoundedText},
    },
    services::{
        artifacts::{documents::read_document, version_ref},
        kernel::{Failure, Refusal, counter},
        usage::accounting,
    },
};

const DISPLAY_ATTEMPTS: u16 = 32;

pub(super) fn read(
    snapshot: &dyn ReadSnapshot,
    context: &InvocationContext,
    version: &Id,
) -> Result<Option<DocumentIndexActivity>, StorageError> {
    let Some(index) = snapshot.documents().document_index(version)? else {
        return Ok(None);
    };
    let bound = reach(snapshot.authority(), context)?.ok_or(StorageError::ScopeDenied)?;
    let project = match &bound {
        Reach::Project(project) => Some(project),
        Reach::Workspace => context.project_id(),
    };
    let entry = snapshot.queue().entry(&index.latest)?;
    let stage = match (index.stage, entry.as_ref().map(|entry| entry.state)) {
        (DocumentIndexStage::Removed, _) => "Removed",
        (_, Some(QueueState::Reconciling)) => "Held",
        (_, Some(QueueState::Claimed)) if entry.as_ref().is_some_and(|e| e.cancel_requested) => {
            "Cancelling"
        }
        (_, Some(QueueState::Failed | QueueState::Dismissed)) => "Failed",
        (_, Some(QueueState::Cancelled)) => "Cancelled",
        (stage, _) => match stage {
            DocumentIndexStage::Queued => "Queued",
            DocumentIndexStage::Extracted => "Indexing",
            DocumentIndexStage::Ready => "Ready",
            DocumentIndexStage::Partial => "Partial",
            DocumentIndexStage::Failed => "Failed",
            DocumentIndexStage::Cancelled => "Cancelled",
            DocumentIndexStage::Removed => "Removed",
        },
    };
    let (indexed_at_ms, chunk_count) = match &index.published {
        Some((delivery, revision)) => (
            snapshot
                .documents()
                .document_publication_time(delivery)?
                .map(counter),
            snapshot
                .documents()
                .document_checkpoint(version, revision)?
                .map(|checkpoint| counter(i64::from(checkpoint.blocks))),
        ),
        None => (None, None),
    };
    let mut ids =
        snapshot
            .documents()
            .document_version_attempts(version, project, DISPLAY_ATTEMPTS + 1)?;
    let more_attempts = ids.len() > usize::from(DISPLAY_ATTEMPTS);
    ids.truncate(DISPLAY_ATTEMPTS.into());
    let attempts = ids
        .iter()
        .map(|id| attempt(snapshot, id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(DocumentIndexActivity {
        run_id: index.latest,
        stage: code(stage),
        indexed_at_ms,
        chunk_count,
        attempts: BoundedList(attempts),
        more_attempts,
    }))
}

fn attempt(snapshot: &dyn ReadSnapshot, id: &Id) -> Result<DocumentIndexAttempt, StorageError> {
    let attempt = snapshot
        .usage()
        .inference_attempt(id)?
        .ok_or(StorageError::ViewUnavailable)?;
    let balance = snapshot
        .usage()
        .attempt_balance(id)?
        .ok_or(StorageError::ViewUnavailable)?;
    let usage = accounting::summary(&balance)?.meter.quantities;
    Ok(DocumentIndexAttempt {
        attempt_id: attempt.attempt_id,
        project_id: attempt.project_id,
        purpose: Id::new(attempt.purpose).map_err(invalid)?,
        model_id: attempt.catalog_model_id,
        connection_id: attempt.connection_id,
        endpoint_profile: Id::new(attempt.endpoint_profile).map_err(invalid)?,
        provider_model: attempt
            .provider_model_id
            .filter(|model| !model.is_empty())
            .map(|model| BoundedText(model.chars().take(256).collect())),
        input_tokens: usage.input_tokens.map(counter),
        output_tokens: usage.output_tokens.map(counter),
        priced_micro_usd: balance.priced_micro_usd.map(counter),
        pricing_classification: code(balance.classification.as_str()),
        monetary_complete: balance.monetary_complete,
        state: code(attempt.state.as_str()),
    })
}

/// Apply the same exact source and audience on the gateway's publication snapshot.
pub(super) fn check_publication(
    context: &InvocationContext,
    query: &AdmittedQuery,
    answer: &QueryAnswer,
    snapshot: &dyn ReadSnapshot,
) -> Result<Option<Refusal>, StorageError> {
    let (
        OperationInput::Documents(ops::Input::Read(request)),
        OperationResult::DocumentsRead {
            value: DocumentResult::Current(detail),
        },
    ) = (query.input(), &answer.result)
    else {
        return Err(invalid("mismatched document activity publication"));
    };
    let ArtifactQuery::Read {
        artifact_id,
        version_id,
    } = &request.query
    else {
        return Err(invalid("mismatched document activity request"));
    };
    let refuse = || {
        Some(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "document",
            Some(artifact_id.clone()),
        ))
    };
    if &snapshot.stamp().store_id != context.store_id()
        || snapshot.stamp().restore_epoch != context.restore_epoch().get()
    {
        return Ok(refuse());
    }
    let artifact = match read_document(
        snapshot.artifacts(),
        snapshot.authority(),
        context,
        artifact_id,
    ) {
        Ok(artifact) => artifact,
        Err(Failure::Refused(_) | Failure::Storage(StorageError::ScopeDenied)) => {
            return Ok(refuse());
        }
        Err(Failure::Storage(error)) => return Err(error),
    };
    if artifact.scope_revision != detail.scope_revision.get()
        || artifact.reference_revision != detail.reference_revision.get()
        || artifact.revision != detail.revision.get()
    {
        return Ok(refuse());
    }
    let version = snapshot
        .artifacts()
        .version(version_id)?
        .filter(|version| version.artifact_id == artifact.id);
    let exact = match (&detail.selected_version, version) {
        (Some(reference), Some(row)) if row.live() => {
            super::indexing::same_source(reference, &version_ref(&row))
        }
        (None, Some(row)) => {
            !row.live() && detail.deleted_selected_version_id.as_ref() == Some(&row.id)
        }
        _ => false,
    };
    Ok(if exact { None } else { refuse() })
}

fn invalid(error: impl ToString) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}
