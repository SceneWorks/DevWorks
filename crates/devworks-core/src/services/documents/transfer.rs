//! Two current primary coordinators authorize one revision-bound document transfer.

use crate::{
    authority::policy::resolve_policy,
    context::{InvocationContext, PrincipalKind},
    gateway::AdmittedQuery,
    operations::{OperationInput, documents as ops},
    ports::{
        IdKind, QueryAnswer,
        storage::{
            ArtifactState, DocumentTransferParty, DocumentTransferProposal, PrincipalBinding,
            ReadSnapshot, StorageError, StoredArtifact, StoredDocumentTransfer, WorkRoot,
            WriteTransaction,
        },
    },
    protocol::{
        DomainErrorCode, Id, Revision, RevisionDimension, RootId,
        records::{DocumentTransferRecord, DocumentTransferState},
        results::OperationResult,
    },
    services::{
        agents::{
            active::{Liveness, SnapshotFacts, TxFacts, activation_is_live, caller_assignment},
            notify_document_transfer,
        },
        artifacts::{ArtifactService, documents::move_document_authorized},
        kernel::{Failure, Refusal, rev},
        runs::is_run_root,
    },
    settings::catalog::AgentRole,
};

fn denied() -> Refusal {
    Refusal::new(DomainErrorCode::PermissionDenied, "document_transfer", None)
}

fn stale() -> Refusal {
    Refusal::new(DomainErrorCode::SourceChanged, "document_transfer", None)
}

fn can_coordinate(binding: &PrincipalBinding, work: &WorkRoot) -> bool {
    let role = if work.kind == "feature" {
        AgentRole::FeatureCoordinator
    } else {
        AgentRole::IssueCoordinator
    };
    is_run_root(work)
        && binding.active
        && binding.kind == PrincipalKind::Agent
        && binding.role == Some(role)
        && binding
            .project_id
            .as_ref()
            .is_none_or(|project| project == &work.project_id)
        && binding
            .tool_families
            .as_ref()
            .is_none_or(|families| families.iter().any(|family| family == "documents"))
}

/// Load the current primary coordinator from host-owned storage, independent of request claims.
fn coordinator(
    tx: &mut dyn WriteTransaction,
    root: &RootId,
    capability: &'static str,
) -> Result<DocumentTransferParty, Failure> {
    let work = tx.sessions().work_root(root)?.ok_or_else(denied)?;
    let policy = resolve_policy(tx.configuration(), Some(&work.project_id))?;
    if policy.profile.is_none() || !policy.tool_selected(capability, true) {
        return Err(denied().into());
    }
    let project = tx
        .projects()
        .project(&work.project_id)?
        .ok_or_else(denied)?;
    let assignment = tx
        .runs()
        .live_assignment_for_work(root)?
        .filter(|assignment| assignment.root_id == *root && assignment.parent_id.is_none())
        .ok_or_else(denied)?;
    let liveness = Liveness::of_tx(tx);
    let activation = tx
        .runs()
        .activation(assignment.execution_id.as_ref().ok_or_else(denied)?)?
        .filter(|activation| {
            activation.root_id == *root && activation_is_live(&liveness, activation)
        })
        .ok_or_else(denied)?;
    let session = tx
        .sessions()
        .session(&assignment.session_id)?
        .ok_or_else(denied)?;
    let binding = tx
        .authority()
        .principal(&session.agent_id)?
        .ok_or_else(denied)?;
    if project.lifecycle != "Active"
        || session.closed()
        || session.project_id.as_ref() != Some(&work.project_id)
        || session.root_id.as_ref() != Some(root)
        || !can_coordinate(&binding, &work)
    {
        return Err(denied().into());
    }
    Ok(DocumentTransferParty {
        project: work.project_id,
        archive_epoch: project.archive_epoch,
        root: root.clone(),
        assignment: assignment.id,
        assignment_generation: assignment.generation,
        session: session.id,
        activation: activation.id,
        activation_owner_epoch: activation.owner_epoch,
        actor: session.agent_id,
        authority_revision: binding.revision,
    })
}

fn speaks_for(ctx: &InvocationContext, party: &DocumentTransferParty) -> bool {
    ctx.principal_kind() == PrincipalKind::Agent
        && ctx.principal() == &party.actor
        && ctx.project_id() == Some(&party.project)
        && ctx.root_id() == Some(&party.root)
        && ctx.assignment_id() == Some(&party.assignment)
        && ctx.session_id() == Some(&party.session)
        && ctx.activation() == Some((&party.activation, party.activation_owner_epoch))
}

fn exact_revision(
    dimension: RevisionDimension,
    kind: &'static str,
    id: &Id,
    expected: &Revision,
    current: i64,
) -> Result<(), Failure> {
    if expected.get() != current {
        return Err(StorageError::conflict(
            dimension,
            kind,
            id.clone(),
            expected.clone(),
            rev(current),
        )
        .into());
    }
    Ok(())
}

pub(super) fn propose(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::TransferProposeInput,
) -> Result<DocumentTransferRecord, Failure> {
    let source = coordinator(
        tx,
        ctx.root_id().ok_or_else(denied)?,
        "devworks/documents.transfer_propose",
    )?;
    if !speaks_for(ctx, &source) || input.summary.0.trim().is_empty() {
        return Err(denied().into());
    }
    caller_assignment(&mut TxFacts::new(tx), ctx, &source.root)?;
    let artifact = super::checked_document(tx, ctx, &input.document_id)?;
    if artifact.state != ArtifactState::Live || artifact.scope.project() != Some(&source.project) {
        return Err(denied().into());
    }
    exact_revision(
        RevisionDimension::Artifact,
        "document",
        &artifact.id,
        &input.expected_document_revision,
        artifact.revision,
    )?;
    exact_revision(
        RevisionDimension::Scope,
        "artifact_scope",
        &artifact.id,
        &input.expected_scope_revision,
        artifact.scope_revision,
    )?;
    let destination = coordinator(
        tx,
        &input.destination_root_id,
        "devworks/documents.transfer_respond",
    )?;
    if source.project == destination.project || source.actor == destination.actor {
        return Err(denied().into());
    }
    let proposal = DocumentTransferProposal {
        document: artifact.id,
        version: artifact.current_version_id.ok_or_else(stale)?,
        document_revision: input.expected_document_revision.clone(),
        scope_revision: input.expected_scope_revision.clone(),
        source,
        destination,
        source_needs_document: input.source_needs_document,
    };
    let id = tx.new_id(IdKind::Proposal);
    tx.documents().insert_document_transfer(&id, &proposal)?;
    notify_document_transfer(tx, &proposal, &id, &input.summary.0)?;
    Ok(DocumentTransferRecord {
        id,
        revision: rev(1),
        state: DocumentTransferState::Pending,
    })
}

/// The writer checks the exact owner, revision and version even when the destination already has
/// a share. An intervening manual transfer cannot lend authority to the old proposal.
fn document_at_proposal(
    tx: &mut dyn WriteTransaction,
    proposal: &DocumentTransferProposal,
) -> Result<StoredArtifact, Failure> {
    let artifact = tx
        .artifacts()
        .artifact(&proposal.document)?
        .ok_or_else(stale)?;
    if artifact.state != ArtifactState::Live
        || artifact.scope.project() != Some(&proposal.source.project)
        || artifact.current_version_id.as_ref() != Some(&proposal.version)
        || artifact.revision != proposal.document_revision.get()
        || artifact.scope_revision != proposal.scope_revision.get()
    {
        return Err(stale().into());
    }
    Ok(artifact)
}

pub(super) fn respond(
    artifacts: &ArtifactService,
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::TransferRespondInput,
) -> Result<DocumentTransferRecord, Failure> {
    let transfer = tx
        .documents()
        .document_transfer(&input.transfer_id)?
        .ok_or_else(denied)?;
    let proposal = transfer.proposal;
    if !speaks_for(ctx, &proposal.destination) {
        return Err(denied().into());
    }
    caller_assignment(&mut TxFacts::new(tx), ctx, &proposal.destination.root)?;
    if transfer.revision != input.expected_revision
        || !matches!(transfer.state, DocumentTransferState::Pending)
    {
        return Err(stale().into());
    }
    if coordinator(
        tx,
        &proposal.destination.root,
        "devworks/documents.transfer_respond",
    )? != proposal.destination
        || coordinator(
            tx,
            &proposal.source.root,
            "devworks/documents.transfer_propose",
        )? != proposal.source
    {
        return Err(stale().into());
    }
    let document = document_at_proposal(tx, &proposal)?;
    let target = &proposal.destination.project;
    let state = if !input.destination_needs_document {
        DocumentTransferState::Declined
    } else if proposal.source_needs_document {
        artifacts.share_authorized(
            tx,
            ctx,
            document,
            target,
            &proposal.scope_revision,
            Some(&transfer.id),
        )?;
        DocumentTransferState::Shared
    } else {
        move_document_authorized(tx, ctx, document, target, &proposal.scope_revision)?;
        DocumentTransferState::Moved
    };
    tx.documents()
        .resolve_document_transfer(&transfer.id, &input.expected_revision, &state)?;
    Ok(DocumentTransferRecord {
        id: transfer.id,
        revision: rev(2),
        state,
    })
}

fn result(transfer: StoredDocumentTransfer) -> DocumentTransferRecord {
    DocumentTransferRecord {
        id: transfer.id,
        revision: transfer.revision,
        state: transfer.state,
    }
}

/// A current participant can recover the status even when a move removed its ability to read the
/// document. This operation returns only the transfer ID, revision and outcome.
pub(super) fn status(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<DocumentTransferRecord, Failure> {
    let transfer = snapshot
        .documents()
        .document_transfer(id)?
        .ok_or_else(denied)?;
    let party = [&transfer.proposal.source, &transfer.proposal.destination]
        .into_iter()
        .find(|party| {
            ctx.principal_kind() == PrincipalKind::Agent
                && ctx.principal() == &party.actor
                && ctx.project_id() == Some(&party.project)
                && ctx.root_id() == Some(&party.root)
        })
        .ok_or_else(denied)?;
    let assignment = caller_assignment(&mut SnapshotFacts::new(snapshot), ctx, &party.root)?;
    let primary = snapshot.runs().live_assignment_for_work(&party.root)?;
    let work = snapshot
        .sessions()
        .work_root(&party.root)?
        .ok_or_else(denied)?;
    let binding = snapshot
        .authority()
        .principal(ctx.principal())?
        .ok_or_else(denied)?;
    if primary.is_none_or(|primary| {
        primary.id != assignment
            || primary.id != party.assignment
            || primary.session_id != party.session
            || primary.parent_id.is_some()
            || (matches!(transfer.state, DocumentTransferState::Pending)
                && primary.generation != party.assignment_generation)
    }) || work.project_id != party.project
        || !can_coordinate(&binding, &work)
        || binding.revision != party.authority_revision
    {
        return Err(denied().into());
    }
    Ok(result(transfer))
}

pub(super) fn check_publication(
    context: &InvocationContext,
    query: &AdmittedQuery,
    answer: &QueryAnswer,
    snapshot: &dyn ReadSnapshot,
) -> Result<Option<Refusal>, StorageError> {
    let (
        OperationInput::Documents(ops::Input::TransferStatus(request)),
        OperationResult::DocumentsTransferStatus { value },
    ) = (query.input(), &answer.result)
    else {
        return Err(StorageError::InvalidRecord(
            "document transfer publication".into(),
        ));
    };
    match status(snapshot, context, &request.transfer_id) {
        Ok(current) if current.revision == value.revision => Ok(None),
        Ok(_) => Ok(Some(stale())),
        Err(Failure::Refused(refusal)) => Ok(Some(refusal)),
        Err(Failure::Storage(error)) => Err(error),
    }
}
