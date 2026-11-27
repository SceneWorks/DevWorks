//! Index publication admits deterministic analysis under each owning scope.
use super::{
    commands::{AGGREGATE, CONSUMER},
    selection::{self, Selection},
    work::Work,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::{AuthorityFence, IdKind, storage::*};
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::{Counter, EventId, Id, OperationId, OperationState};
use crate::services::kernel::{Failure, rev};
use serde_json::json;

pub const INDEX_CONSUMER: &str = "RepositoryAnalysisIndexPublished";

pub(super) fn index_published(
    store: &dyn ApplicationStore,
    entry: &QueueEntry,
    event: &EventRecord,
    cancellation: &Cancellation,
) -> Result<(), Failure> {
    if event.kind != "IndexPublished"
        || event.actor != store.info().host_actor
        || entry.consumer != INDEX_CONSUMER
        || entry.event_id != event.id
    {
        return Err(StorageError::ScopeDenied.into());
    }
    let repository: Id = serde_json::from_value(event.payload["repositoryId"].clone())
        .map_err(|_| selection::unavailable())?;
    let view: Id = serde_json::from_value(event.payload["viewId"].clone())
        .map_err(|_| selection::unavailable())?;
    let pointer = read_snapshot(store, &ReadOptions::default(), |read| {
        read.indexing().published(&repository)
    })?;
    if pointer.as_ref().is_none_or(|ptr| ptr.view_id != view) {
        return Ok(());
    }
    let event_id = event.id.clone();
    let host = event.actor.clone();
    let offers = read_snapshot(
        store,
        &ReadOptions {
            cancellation: Some(cancellation.clone()),
            ..Default::default()
        },
        |read| {
            Ok((|| -> Result<_, Failure> {
                let mut owners = Vec::new();
                for project in read.projects().repository_projects(&repository)? {
                    if read
                        .projects()
                        .project(&project)?
                        .is_some_and(|row| row.lifecycle == ACTIVE)
                    {
                        owners.push(Some(project));
                    }
                }
                if owners.is_empty() {
                    owners.push(None);
                }
                let mut offers = Vec::new();
                for project in owners {
                    let scope = project.clone().map_or(DataScope::Workspace, |project_id| {
                        DataScope::Project { project_id }
                    });
                    let owner = InvocationContext::new(
                        host.clone(),
                        PrincipalKind::Host,
                        event_id.0.clone(),
                        event_id.0.clone(),
                        read.stamp().store_id.clone(),
                        Counter::new(read.stamp().restore_epoch)
                            .map_err(|_| selection::unavailable())?,
                        rev(1),
                    )
                    .with_lineage(project.clone(), None, None);
                    let selected =
                        Selection::for_publication(read, &owner, &scope, &repository, &view)?;
                    let policy = crate::authority::policy::resolve_policy(
                        read.configuration(),
                        project.as_ref(),
                    )?;
                    let fence = AuthorityFence::Policy {
                        project_id: project,
                        digest: policy.digest(),
                    };
                    offers.push((owner, selected, fence));
                }
                Ok(offers)
            })())
        },
    )??;
    let entry = entry.clone();
    let cancellation = cancellation.clone();
    run_write(store, WriteOptions::host_state(), move |tx| {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let current = tx
            .queue()
            .entry(&entry.id)?
            .ok_or(StorageError::Cancelled)?;
        if current.state != QueueState::Claimed
            || current.fence() != entry.fence()
            || current.cancellation_epoch != entry.cancellation_epoch
            || current.cancel_requested
            || current.claim_boot_id != tx.store().boot_id
            || current
                .lease_until_ms
                .is_none_or(|expires| expires <= tx.now_ms())
        {
            return Err(StorageError::Cancelled);
        }
        if tx
            .indexing()
            .published(&repository)?
            .is_none_or(|ptr| ptr.view_id != view)
        {
            return Ok(());
        }
        let mut owners = Vec::new();
        for project in tx.projects().repository_projects(&repository)? {
            if tx
                .projects()
                .project(&project)?
                .is_some_and(|row| row.lifecycle == ACTIVE)
            {
                owners.push(Some(project));
            }
        }
        if owners.is_empty() {
            owners.push(None);
        }
        if owners.len() != offers.len()
            || owners.iter().any(|owner| {
                !offers
                    .iter()
                    .any(|(context, _, _)| context.project_id() == owner.as_ref())
            })
        {
            return Err(StorageError::Busy);
        }
        for (context, selected, fence) in offers {
            if !crate::services::fences::fences_hold(tx, std::slice::from_ref(&fence))? {
                return Err(StorageError::Busy);
            }
            selected
                .validate_in(tx, &context)
                .map_err(Failure::into_storage)?;
            if selection::configuration_revision(tx.indexing(), &selected.scope)
                .map_err(Failure::into_storage)?
                != selected.configuration_revision
            {
                return Err(StorageError::Busy);
            }
            admit(tx, &context, selected, fence).map_err(Failure::into_storage)?;
        }
        Ok(())
    })
    .map_err(Failure::from)
}

fn admit(
    tx: &mut dyn WriteTransaction,
    context: &InvocationContext,
    selected: Selection,
    fence: AuthorityFence,
) -> Result<(), Failure> {
    let kind = AnalysisCommand::IndexPublished;
    let exact = tx
        .analysis()
        .analysis_runs(&selected.binding_key, false, None, 32)?;
    if exact.iter().any(|run| {
        run.intent.command == kind
            && !matches!(
                run.state,
                OperationState::Failed | OperationState::CancelledBeforeDispatch
            )
    }) {
        return Ok(());
    }
    let lineage = tx
        .analysis()
        .analysis_runs(&selected.lineage_key, true, None, 32)?;
    for run in lineage {
        if run.intent.command == kind && run.state == OperationState::IntentRecorded {
            supersede(tx, run)?;
        }
    }
    let id = tx.new_id(IdKind::Operation);
    let operation = OperationId(tx.new_id(IdKind::Operation));
    let delivery = tx.new_id(IdKind::QueueEntry);
    let event = EventId(tx.new_id(IdKind::Event));
    let pin = selected.retain_in(tx, context)?;
    let owner = AnalysisOwner {
        actor: context.principal().clone(),
        kind: PrincipalKind::Host.as_str().into(),
        invocation: context.invocation_id().clone(),
        ingress: context.ingress_namespace().clone(),
        project: context.project_id().cloned(),
        root: None,
        assignment: None,
        session: None,
        policy_revision: context.policy_revision().get(),
        restore_epoch: context.restore_epoch().get(),
        namespace: super::super::lease::owner(context),
        activation: None,
        worktree: None,
        fences: vec![fence],
    };
    let mut phases = vec![
        AnalysisPhase::Links,
        AnalysisPhase::Vitals,
        AnalysisPhase::Security,
    ];
    if selected.views.len() > 1 {
        phases.push(AnalysisPhase::Communities);
    }
    let now = tx.now_ms();
    let work = Work {
        run_id: id.clone(),
        delivery_id: delivery.clone(),
        authority: owner.clone(),
    };
    let run = StoredAnalysisRun {
        id,
        operation: operation.clone(),
        active_operation: operation.clone(),
        pin_id: pin,
        owner,
        intent: AnalysisIntent {
            command: kind,
            scope: selected.scope.clone(),
            selection: ViewSelection::Published {
                repository_ids: selected
                    .views
                    .iter()
                    .map(|view| view.repository_id.clone())
                    .collect(),
            },
            witnesses: selected.witnesses().to_vec(),
            repository_lineages: selected.repository_lineages.clone(),
            options_digest: selection::digest(&json!({"kind": kind})),
            views: selected.views,
            unavailable: selected.unavailable,
            configuration_revision: selected.configuration_revision,
            node_ids: Vec::new(),
            batch_limit: 32,
            link_mode: None,
            refresh_mode: None,
            include_all_source: false,
            prerequisite_run: None,
        },
        binding_key: selected.binding_key,
        lineage_key: selected.lineage_key,
        state: OperationState::IntentRecorded,
        revision: 1,
        created_at_ms: now,
        updated_at_ms: now,
        phases: phases
            .into_iter()
            .map(|phase| StoredAnalysisPhase {
                phase,
                state: OperationState::IntentRecorded,
                operation: None,
                report: None,
                diagnostic: None,
                finished_at_ms: None,
                subjects: Vec::new(),
            })
            .collect(),
    };
    tx.operations().record_intent(NewOperation {
        id: operation.clone(),
        root_id: None,
        assignment_id: None,
        capability: "analysis.index_published".into(),
        schema_version: 3,
        args_digest: run.binding_key.clone(),
        payload: serde_json::to_value(work)
            .map_err(|_| StorageError::InvalidRecord("analysis work custody".into()))?,
    })?;
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: context.principal().clone(),
        project_id: context.project_id().cloned(),
        root_id: None,
        operation_id: Some(operation.clone()),
        aggregate_kind: AGGREGATE.into(),
        aggregate_id: run.id.as_str().into(),
        aggregate_revision: 1,
        kind: "RepositoryAnalysisRequested".into(),
        payload: json!({"runId": run.id, "kind": kind.key(), "binding": run.binding_key}),
    })?;
    tx.analysis().insert_analysis_run(&run)?;
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: None,
        operation_id: Some(operation),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({"runId":run.id}),
    })?;
    Ok(())
}

fn supersede(tx: &mut dyn WriteTransaction, prior: StoredAnalysisRun) -> Result<(), Failure> {
    let Some(operation) = tx.operations().operation(&prior.active_operation)? else {
        return Ok(());
    };
    if operation.state != OperationState::IntentRecorded || !operation.attempts.is_empty() {
        return Ok(());
    }
    let work = Work::from_operation(&operation)?;
    let Some(delivery) = tx.queue().entry(&work.delivery_id)? else {
        return Ok(());
    };
    if delivery.state != QueueState::Ready {
        return Ok(());
    }
    tx.queue().cancel(&delivery.id)?;
    super::commands::cancelled(tx, prior, "SupersededByIndexPublication")?;
    Ok(())
}
