//! Source-owned document transcription through the ordinary native provider and usage ledger.
use crate::providers::driver::{NativeInvocation, NativeProviders};
use devworks_core::{
    context::InvocationContext,
    ports::{
        storage::{
            ApplicationStore, AttemptState, Cancellation, ReadOptions, StorageError, read_snapshot,
        },
        usage::{PriceQuery, PriceResolver},
    },
    protocol::{
        Counter, Id, OperationState, Protocol, Revision,
        provider::{
            Attribution, InferenceInput, InferenceOptions, InferencePurpose, InferenceRequest,
            ModelSelection,
        },
        records::{EffortName, RouteIntent, TierName},
    },
    services::{
        artifacts::ArtifactService,
        documents::vision::{
            DocumentVision, MAX_TRANSCRIPTION_BYTES, REQUEST_DEADLINE_MS, TRANSCRIPTION_TOKENS,
            VisionEvidence, VisionExecution, VisionInvocation,
        },
        prices::CatalogPriceResolver,
        router::{
            AuxiliaryRequest, InferenceRouter, Purpose, RequestAdmission,
            accounting::{AttemptSelection, PinFence, ReportCategory},
        },
        usage::{
            accounting::{self, AttemptEnd, AttemptRequest},
            auxiliary,
            meter::InputSemantics,
            pricing::PricingContext,
        },
    },
};
use serde_json::json;
use std::sync::{Arc, Weak};

pub struct NativeVision {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    router: Arc<InferenceRouter>,
    providers: Weak<NativeProviders>,
    runtime: tokio::runtime::Handle,
}

impl NativeVision {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        router: Arc<InferenceRouter>,
        providers: &Arc<NativeProviders>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            store,
            artifacts,
            router,
            providers: Arc::downgrade(providers),
            runtime,
        }
    }

    fn plan(
        &self,
        ctx: &InvocationContext,
        work: &VisionInvocation,
    ) -> Result<Option<NativeInvocation>, StorageError> {
        let info = self.store.info();
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let intent = RouteIntent::Tier {
                tier: TierName::Low,
            };
            let required_capabilities = vec!["vision".to_owned()];
            let Ok(route) = self.router.route_auxiliary(
                read,
                &AuxiliaryRequest {
                    purpose: Purpose::Vision,
                    session_id: ctx.session_id().cloned(),
                    intent: intent.clone(),
                    effort: EffortName::Default,
                    required_capabilities: required_capabilities.clone(),
                },
            )?
            else {
                return Ok(None);
            };
            // This profile cannot enforce the bounded output sent for an individual image.
            if route.endpoint_profile == "CodexSubscriptionResponses" {
                return Ok(None);
            }
            let owner = match auxiliary::ownership(read, &info, ctx) {
                Ok(owner) => owner,
                Err(auxiliary::AuxiliaryError::Unavailable(_)) => return Ok(None),
                Err(auxiliary::AuxiliaryError::Storage(error)) => return Err(error),
            };
            let pin = match route.pin_id {
                Some(id) => {
                    let Some(session) = ctx.session_id() else {
                        return Err(StorageError::ScopeDenied);
                    };
                    let current = read
                        .router()
                        .session_pin(session)?
                        .filter(|pin| pin.pin_id == id)
                        .ok_or(StorageError::ScopeDenied)?;
                    Some(PinFence {
                        id,
                        revision: current.revision,
                    })
                }
                None => None,
            };
            let control = ctx
                .root_id()
                .map(|root| read.runs().root_control(root))
                .transpose()?
                .flatten();
            let (generation, instructions, stop_epoch) = control.map_or((1, 1, 0), |row| {
                (row.generation, row.instruction_revision, row.stop_epoch)
            });
            let revision =
                |value| Revision::new(value).map_err(|_| invalid("document vision revision"));
            let provider = PriceQuery {
                catalog_model_id: route.model_id.clone(),
                provider_model_id: Some(route.upstream_model_id),
                connection_id: route.connection_id.clone(),
                endpoint_profile: route.endpoint_profile,
                service_tier: None,
                input_tokens: None,
                cache_write_ttl_ms: None,
                processing_region: None,
                required_modalities: vec!["Text".into(), "Image".into()],
            };
            let request = InferenceRequest {
                options: InferenceOptions {
                    selection: ModelSelection::Model {
                        model_id: route.model_id.clone(),
                    },
                    effort: None,
                    output_schema: None,
                    deadline_ms: REQUEST_DEADLINE_MS,
                },
                authority_revision: ctx.policy_revision().clone(),
                control_generation: revision(generation)?,
                instruction_revision: revision(instructions.max(1))?,
                cancellation_revision: revision(stop_epoch.saturating_add(1))?,
                protocol: Protocol::Draft1,
                request_id: work.request.clone(),
                purpose: InferencePurpose::Vision,
                attribution: Attribution::Service {
                    operation_id: work.material.operation.clone(),
                    project_id: ctx.project_id().cloned(),
                },
                input: InferenceInput::Generation {
                    session_id: ctx
                        .session_id()
                        .cloned()
                        .unwrap_or_else(|| work.material.operation.0.clone()),
                    context: work.material.context.clone(),
                    context_epoch: Counter::new(owner.expected_context_epoch.unwrap_or(0))
                        .map_err(|_| invalid("document vision context epoch"))?,
                    tools: vec![],
                    response_headroom_tokens: revision(i64::from(TRANSCRIPTION_TOKENS))?,
                    requires_images: true,
                    requires_parallel_tools: false,
                },
            };
            let logical = RequestAdmission {
                request_id: work.request.clone(),
                purpose: Purpose::Vision,
                session_id: ctx.session_id().cloned(),
                project_id: ctx.project_id().cloned(),
                root_id: ctx.root_id().cloned(),
                model_id: route.model_id.clone(),
                connection_id: route.connection_id,
                pin_id: pin.as_ref().map(|pin| pin.id.clone()),
                args_digest: String::new(),
            };
            let attempt = AttemptRequest {
                admission_key: work.request.clone(),
                request_id: work.request.clone(),
                actor_id: ctx.principal().clone(),
                work_id: owner.work_id,
                job_run_id: owner.job_run_id,
                role: owner.role,
                quote: CatalogPriceResolver.quote(read, &provider)?,
                price_generation: read.usage().price_revision()?,
                override_revision: read.usage().price_override_revision(&route.model_id)?,
                query: provider,
                pricing: PricingContext {
                    input_semantics: InputSemantics::Unknown,
                    cache_read_possible: false,
                    cache_write_possible: false,
                    cache_write_ttl_ms: None,
                },
                selection: AttemptSelection::Conversation {
                    intent,
                    effort: EffortName::Default,
                    required_capabilities,
                    pin,
                },
                // Reserve bounded image-token headroom. The ledger settles from observed usage.
                input_tokens: work.material.context.byte_length.get().saturating_add(4096),
                output_headroom: i64::from(TRANSCRIPTION_TOKENS),
                unit_headroom: vec![],
                expected_context_epoch: owner.expected_context_epoch,
                dispatch: owner.dispatch,
                cache_diagnostics: vec![],
            };
            Ok(Some(NativeInvocation {
                source_fences: work.fences.clone(),
                instruction_targets: Some(vec![]),
                context: ctx.clone(),
                logical,
                category: ReportCategory::Other,
                attempt,
                request,
                nested: None,
            }))
        })
    }

    fn recorded(
        &self,
        providers: &NativeProviders,
        ctx: &InvocationContext,
        work: &VisionInvocation,
    ) -> Result<Option<VisionExecution>, StorageError> {
        let record = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let Some(route) = read.router().route_request(&work.request)? else {
                return Ok(None);
            };
            let operation = read
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if route.purpose != Purpose::Vision.as_str()
                || operation.payload["actorId"] != json!(ctx.principal())
                || route.project_id.as_ref() != ctx.project_id()
            {
                return Err(StorageError::ScopeDenied);
            }
            let mut owned = None;
            for journal in &operation.attempts {
                if let Some(attempt) = read.usage().inference_attempt(&journal.id.0)? {
                    if accounting::snapshot(&attempt)?.admission_key != work.request
                        || attempt.actor_id != *ctx.principal()
                        || attempt.purpose != Purpose::Vision.as_str()
                        || attempt.request_id != work.request
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    let balance = read
                        .usage()
                        .attempt_balance(&attempt.attempt_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    owned = Some((attempt, balance));
                }
            }
            Ok(Some((operation.state, owned)))
        })?;
        let Some((state, owned)) = record else {
            return Ok(None);
        };
        let Some((attempt, balance)) = owned else {
            return Ok(Some(if state == OperationState::IntentRecorded {
                VisionExecution::Unavailable
            } else {
                VisionExecution::Unknown
            }));
        };
        if state == OperationState::CancelledBeforeDispatch
            || attempt.state == AttemptState::CancelledBeforeDispatch
        {
            return Ok(Some(failed("VisionCancelled")));
        }
        if !balance.local_quiescent
            || balance.unknown_tail
            || attempt.state != AttemptState::Settled
        {
            return Ok(Some(VisionExecution::Unknown));
        }
        let result = providers.recorded_result(&work.request, &work.request)?;
        if result.outcome != Some(AttemptEnd::Completed) {
            let reason = if result.faults.iter().any(|fault| fault == "VisionTextLimit") {
                "VisionTextLimit"
            } else {
                "VisionResultIncomplete"
            };
            return Ok(Some(failed(reason)));
        }
        let Some(output) = result.result_artifact else {
            return Ok(Some(VisionExecution::Unknown));
        };
        if result.attempt_id.as_ref().map(|id| &id.0) != Some(&attempt.attempt_id)
            || result.source_context.as_ref().map(|r| json!(r))
                != Some(json!(work.material.context))
        {
            return Err(StorageError::ScopeDenied);
        }
        let (_, provenance) = self
            .artifacts
            .read_exact(
                self.store.as_ref(),
                ctx,
                &output,
                MAX_TRANSCRIPTION_BYTES as u64,
            )
            .map_err(crate::turn_error::artifact_read_fault)?;
        if provenance["producedBy"] != json!(self.store.info().host_actor)
            || provenance["derived"]["kind"] != "DocumentVisionOutput"
            || provenance["derived"]["requestId"] != json!(work.request)
            || provenance["derived"]["attemptId"] != json!(attempt.attempt_id)
            || provenance["derived"]["documentSource"] != json!(work.document)
        {
            return Err(StorageError::ScopeDenied);
        }
        let upstream_model = attempt
            .provider_model_id
            .ok_or_else(|| invalid("missing vision provider model"))?;
        Ok(Some(VisionExecution::Recorded(Box::new(VisionEvidence {
            operation: attempt.operation_id,
            request: work.request.clone(),
            attempt: attempt.attempt_id,
            model: attempt.catalog_model_id,
            connection: attempt.connection_id,
            upstream_model,
            output,
        }))))
    }
}

impl DocumentVision for NativeVision {
    fn execute(
        &self,
        ctx: &InvocationContext,
        work: &VisionInvocation,
        cancellation: &Cancellation,
        observe_only: bool,
    ) -> Result<VisionExecution, StorageError> {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let Some(providers) = self.providers.upgrade() else {
            return Ok(if observe_only {
                VisionExecution::Unknown
            } else {
                VisionExecution::Unavailable
            });
        };
        if let Some(prior) = self.recorded(&providers, ctx, work)? {
            return Ok(prior);
        }
        if observe_only {
            return Ok(VisionExecution::Unknown);
        }
        let Some(plan) = self.plan(ctx, work)? else {
            return Ok(VisionExecution::Unavailable);
        };
        let outcome = self
            .runtime
            .block_on(providers.produce_cancellable(plan, Some(cancellation.clone())));
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if let Some(closed) = self.recorded(&providers, ctx, work)? {
            return Ok(closed);
        }
        match outcome {
            Err(error @ (StorageError::Busy | StorageError::CommitUnknown)) => Err(error),
            Err(StorageError::ScopeDenied | StorageError::Cancelled) | Ok(_) => {
                Ok(VisionExecution::Unavailable)
            }
            Err(error) => Err(error),
        }
    }
}

fn failed(id: &str) -> VisionExecution {
    VisionExecution::Failed {
        diagnostic: Id::new(id).expect("static vision diagnostic"),
    }
}

fn invalid(note: &str) -> StorageError {
    StorageError::InvalidRecord(note.into())
}
