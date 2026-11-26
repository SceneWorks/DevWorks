//! Dispatcher composition for native repository analysis.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
use devworks_core::services::graph::repository_analysis::{AnalysisService, CONSUMER, Execution};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

struct Effects {
    service: Arc<AnalysisService>,
    store: Arc<dyn ApplicationStore>,
}

impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let store = self.store.clone();
        let service = self.service.clone();
        Box::pin(async move {
            let cancellation = CancelOnDrop(Cancellation::new());
            let worker_cancel = cancellation.0.clone();
            let delivery = operation.payload["deliveryId"]
                .as_str()
                .and_then(|id| Id::new(id).ok());
            let mut worker =
                tokio::task::spawn_blocking(move || service.execute(&operation, &worker_cancel));
            let mut heartbeat = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    result = &mut worker => return result.map(effect)
                        .unwrap_or_else(|_| EffectResult::Unknown("analysis worker stopped before observation".into())),
                    _ = heartbeat.tick() => {
                        let (store, delivery) = (store.clone(), delivery.clone());
                        let stopped = tokio::task::spawn_blocking(move || {
                            read_snapshot(store.as_ref(), &ReadOptions::default(), |read| {
                                Ok(match delivery {
                                    Some(id) => read.queue().entry(&id)?
                                        .is_none_or(|entry| entry.cancel_requested || entry.state != QueueState::Claimed),
                                    None => true,
                                })
                            })
                        }).await;
                        if !matches!(stopped, Ok(Ok(false))) {
                            cancellation.0.cancel();
                        }
                    }
                }
            }
        })
    }

    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let service = self.service.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || effect(service.observe(&operation)))
                .await
                .unwrap_or_else(|_| EffectResult::Unknown("analysis observer stopped".into()))
        })
    }

    fn completion_current(
        &self,
        tx: &mut dyn WriteTransaction,
        operation: &OperationRecord,
        _: &Value,
    ) -> Result<bool, StorageError> {
        self.service.completion_current(tx, operation)
    }
}

fn effect(execution: Execution) -> EffectResult {
    match execution {
        Execution::Completed(evidence) => EffectResult::Completed(evidence),
        Execution::Failed(cause) => EffectResult::Failed(cause.into()),
        Execution::NotApplied(cause) => EffectResult::NotApplied(cause.into()),
        Execution::Unknown(cause) => EffectResult::Unknown(cause.into()),
    }
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Settled;
impl DeliveryHandler for Settled {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}

pub(crate) fn consumer(
    store: Arc<dyn ApplicationStore>,
    service: Arc<AnalysisService>,
) -> Consumer {
    Consumer {
        name: CONSUMER.into(),
        handler: Arc::new(Settled),
        effect: Some(Arc::new(Effects { store, service })),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5000,
        retry: RetryPolicy::default(),
        max_in_flight: Some(2),
    }
}

use crate::{mcp_sampling::McpSampling, providers::driver::NativeProviders};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::mcp::{SamplingCall, SamplingMessage};
use devworks_core::ports::{AuthorityFence, IdKind};
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::{AttemptId, Counter, provider::ToolCallKey};
use devworks_core::services::{
    artifacts::ArtifactService,
    graph::repository_analysis::{
        AnalysisModel, AnalysisModelInput, MAX_OUTPUT_TOKENS, ModelObservation, ProviderAnalysis,
    },
    mcp::SamplingGrant,
    router::InferenceRouter,
    usage::accounting::AttemptEnd,
};
use std::sync::Weak;

/// The router and its normal provider codecs remain the only producer of paid analysis results.
pub(crate) struct Model {
    pub store: Arc<dyn ApplicationStore>,
    pub artifacts: Arc<ArtifactService>,
    pub providers: Weak<NativeProviders>,
    pub router: Arc<InferenceRouter>,
    pub runtime: tokio::runtime::Handle,
}

impl AnalysisModel for Model {
    fn generate(&self, mut request: AnalysisModelInput) -> ModelObservation {
        let Some(providers) = self.providers.upgrade() else {
            return ModelObservation::Refused("NativeProviderUnavailable");
        };
        let Ok(ctx) = self.context(&request) else {
            return ModelObservation::Refused("ModelAuthorityUnavailable");
        };
        let recorded = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            Ok(read.router().route_request(&request.request)?.is_some())
        });
        match recorded {
            Ok(true) => return self.observe(&ctx, &request.request),
            Ok(false) => {}
            Err(_) => return ModelObservation::Unknown,
        }
        request.fences.push(AuthorityFence::AnalysisRequest {
            operation: request.analysis_operation.clone(),
            request: request.request.clone(),
        });
        let grant = SamplingGrant {
            context: ctx.clone(),
            operation_id: request.analysis_operation,
            attempt_id: request.request.clone(),
            call: ToolCallKey {
                attempt_id: AttemptId(request.request.clone()),
                native_call_id: request.request.as_str().to_owned(),
                ordinal: Counter::new(0).expect("zero"),
            },
            fences: request.fences,
        };
        let call = SamplingCall {
            ordinal: 1,
            messages: vec![SamplingMessage {
                assistant: false,
                text: request.prompt,
            }],
            system_prompt: Some(request.system.into()),
            max_tokens: MAX_OUTPUT_TOKENS,
            temperature: None,
            stop_sequences: Vec::new(),
        };
        let sampling = McpSampling::for_analysis(
            self.store.clone(),
            self.artifacts.clone(),
            providers,
            self.router.clone(),
            request.request.clone(),
        );
        let delivery =
            self.runtime
                .block_on(sampling.produce_analysis(&grant, &call, request.cancellation));
        let observed = self.observe(&ctx, &request.request);
        match (observed, delivery) {
            (
                ModelObservation::NotApplied,
                Err(
                    devworks_core::ports::mcp::SamplingRefusal::Failed(reason)
                    | devworks_core::ports::mcp::SamplingRefusal::InvalidRequest(reason)
                    | devworks_core::ports::mcp::SamplingRefusal::Unauthorized(reason),
                ),
            ) => ModelObservation::Refused(reason),
            (observed, _) => observed,
        }
    }

    fn observe(&self, context: &InvocationContext, request: &Id) -> ModelObservation {
        let Some(providers) = self.providers.upgrade() else {
            return ModelObservation::Unknown;
        };
        let record = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            read.router().route_request(request)
        });
        match record {
            Ok(None) => return ModelObservation::NotApplied,
            Ok(Some(record))
                if context.principal_kind() != PrincipalKind::Human
                    && (context.session_id() != record.session_id.as_ref()
                        || context.project_id() != record.project_id.as_ref()) =>
            {
                return ModelObservation::Unknown;
            }
            Err(_) => return ModelObservation::Unknown,
            _ => {}
        }
        let Ok(result) = providers.recorded_result(request, request) else {
            return ModelObservation::Unknown;
        };
        match result.outcome {
            Some(AttemptEnd::Completed) => {
                let (Some(artifact), Some(binding), Some(attempt)) =
                    (result.result_artifact, result.binding, result.attempt_id)
                else {
                    return ModelObservation::Unknown;
                };
                let Ok((bytes, _)) =
                    self.artifacts
                        .read_exact(self.store.as_ref(), context, &artifact, 64 * 1024)
                else {
                    return ModelObservation::Unknown;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    return ModelObservation::Failed;
                };
                ModelObservation::Completed(ProviderAnalysis {
                    text,
                    request: request.clone(),
                    attempt: attempt.0,
                    model: binding.model_id.as_str().into(),
                })
            }
            Some(
                AttemptEnd::Failed
                | AttemptEnd::Cancelled
                | AttemptEnd::Incomplete
                | AttemptEnd::ToolCallsReady,
            ) => ModelObservation::Failed,
            Some(AttemptEnd::UnknownTail) | None => ModelObservation::Unknown,
        }
    }
}

impl Model {
    fn context(&self, request: &AnalysisModelInput) -> Result<InvocationContext, StorageError> {
        if request.context.principal_kind() != PrincipalKind::Human {
            return Ok(request.context.clone());
        }
        let project = match &request.scope {
            DataScope::Workspace => None,
            DataScope::Project { project_id } => Some(project_id.clone()),
        };
        let actor = request.context.principal().clone();
        let root = request.context.root_id().cloned();
        let digest = sha256_hex(
            format!(
                "{}\0{}\0{}",
                actor.0.as_str(),
                project.as_ref().map_or("", |project| project.0.as_str()),
                root.as_ref().map_or("", |root| root.0.as_str()),
            )
            .as_bytes(),
        );
        let session = Id::new(format!("{HOST_SESSION_PREFIX}analysis-{digest}"))
            .expect("qualified analysis session ID");
        let ctx = request
            .context
            .clone()
            .with_lineage(
                project.clone(),
                root.clone(),
                request.context.assignment_id().cloned(),
            )
            .with_session(session.clone());
        let op = request.analysis_operation.clone();
        let subject = request.request.clone();
        let admissible = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            devworks_core::services::graph::repository_analysis::authorize_model_request(
                read, &ctx, &op, &subject,
            )
        })?;
        if !admissible {
            return Err(StorageError::ScopeDenied);
        }
        let opened = ctx.clone();
        let fences = request.fences.clone();
        run_write(self.store.as_ref(), WriteOptions::host_state(), move |tx| {
            let gate = devworks_core::ports::Authorization::direct()
                .fenced_by(fences)
                .fenced_by(vec![AuthorityFence::AnalysisRequest {
                    operation: op,
                    request: subject,
                }]);
            if tx.actors().actor_kind(&actor)? != Some(PrincipalKind::Human)
                || !devworks_core::services::fences::authorization_holds_in(tx, &opened, &gate)?
                || project
                    .as_ref()
                    .is_some_and(|_| opened.project_id() != project.as_ref())
            {
                return Err(StorageError::ScopeDenied);
            }
            if let Some(project) = &project
                && !tx
                    .projects()
                    .project(project)?
                    .is_some_and(|row| row.lifecycle == ACTIVE)
            {
                return Err(StorageError::ScopeDenied);
            }
            if let Some(root) = &root
                && !tx
                    .sessions()
                    .work_root(root)?
                    .is_some_and(|work| Some(&work.project_id) == project.as_ref())
            {
                return Err(StorageError::ScopeDenied);
            }
            match tx.sessions().session(&session)? {
                Some(row)
                    if !row.closed()
                        && row.agent_id == actor
                        && row.created_by == actor
                        && row.project_id == project
                        && row.root_id == root =>
                {
                    Ok(())
                }
                Some(_) => Err(StorageError::ScopeDenied),
                None => {
                    let now = tx.now_ms();
                    let conversation = tx.new_id(IdKind::Conversation);
                    tx.sessions().insert_session(&StoredSession {
                        id: session,
                        conversation_id: conversation,
                        agent_id: actor.clone(),
                        project_id: project,
                        root_id: root,
                        title: "Repository analysis".into(),
                        context_epoch: 0,
                        revision: 1,
                        created_by: actor,
                        created_at_ms: now,
                        closed_at_ms: None,
                        closed_by: None,
                        next_sequence: 1,
                        checkpoint_sequence: 0,
                    })
                }
            }
        })?;
        Ok(ctx)
    }
}

struct IndexPublished(Arc<AnalysisService>);
impl crate::dispatch::AsyncDeliveryHandler for IndexPublished {
    fn start(&self, delivery: crate::dispatch::AsyncDelivery) -> crate::dispatch::AsyncFuture {
        use crate::dispatch::AsyncOutcome;
        let mut cancelled = delivery.cancelled;
        let service = self.0.clone();
        Box::pin(async move {
            let cancellation = CancelOnDrop(Cancellation::new());
            let token = cancellation.0.clone();
            let event = delivery.delivery;
            let mut worker = tokio::task::spawn_blocking(move || {
                service.index_published(&event.entry, &event.event, &token)
            });
            let result = tokio::select! {
                result = &mut worker => result,
                _ = cancelled.changed() => {
                    cancellation.0.cancel();
                    worker.await
                },
            };
            match result {
                Ok(Ok(())) => AsyncOutcome::Committed,
                Ok(Err(StorageError::ScopeDenied | StorageError::InvalidRecord(_))) => {
                    AsyncOutcome::Poison("Index publication testimony failed validation".into())
                }
                _ => AsyncOutcome::Retry(
                    "Analysis admission could not verify the current index".into(),
                ),
            }
        })
    }
}
pub(crate) fn index_consumer(service: Arc<AnalysisService>) -> Consumer {
    Consumer {
        name: devworks_core::services::graph::repository_analysis::INDEX_CONSUMER.into(),
        handler: Arc::new(Settled),
        effect: None,
        async_handler: Some(Arc::new(IndexPublished(service))),
        eligibility: None,
        stop_grace_ms: 5000,
        retry: RetryPolicy::default(),
        max_in_flight: Some(2),
    }
}

struct RepositoryDocumentation {
    git: Arc<devworks_core::services::git::GitService>,
    authority: Arc<dyn devworks_core::ports::AuthorityPort>,
}

impl crate::dispatch::AsyncDeliveryHandler for RepositoryDocumentation {
    fn start(&self, delivery: crate::dispatch::AsyncDelivery) -> crate::dispatch::AsyncFuture {
        use crate::dispatch::AsyncOutcome;
        use devworks_core::protocol::{CommandReply, results::DomainDecision};
        let (git, authority) = (self.git.clone(), self.authority.clone());
        let mut cancelled = delivery.cancelled;
        Box::pin(async move {
            let cancellation = CancelOnDrop(Cancellation::new());
            let token = cancellation.0.clone();
            let claimed = delivery.delivery;
            let mut worker = tokio::task::spawn_blocking(move || {
                git.admit_repository_documentation(
                    &*authority,
                    &claimed.entry,
                    &claimed.event,
                    &token,
                )
            });
            let result = tokio::select! {
                result = &mut worker => result,
                _ = cancelled.changed() => {
                    cancellation.0.cancel();
                    worker.await
                },
            };
            match result {
                Ok(Ok(CommandReply::Recorded { receipt, .. })) => match receipt.decision {
                    DomainDecision::Refused { .. } => AsyncOutcome::Hold(
                        "Repository documentation was refused by the current owner policy".into(),
                    ),
                    _ => AsyncOutcome::Committed,
                },
                Ok(Err(
                    devworks_core::services::kernel::Failure::Refused(_)
                    | devworks_core::services::kernel::Failure::Storage(
                        StorageError::ScopeDenied | StorageError::InvalidRecord(_),
                    ),
                )) => AsyncOutcome::Poison(
                    "Repository documentation lost its exact synthesis or worktree".into(),
                ),
                _ => AsyncOutcome::Retry(
                    "Repository documentation admission could not be recorded".into(),
                ),
            }
        })
    }
}

pub(crate) fn documentation_consumer(
    git: Arc<devworks_core::services::git::GitService>,
    authority: Arc<dyn devworks_core::ports::AuthorityPort>,
) -> Consumer {
    Consumer {
        name: devworks_core::services::graph::repository_analysis::DOCS_CONSUMER.into(),
        handler: Arc::new(Settled),
        effect: None,
        async_handler: Some(Arc::new(RepositoryDocumentation { git, authority })),
        eligibility: None,
        stop_grace_ms: 5000,
        retry: RetryPolicy::default(),
        max_in_flight: Some(2),
    }
}
