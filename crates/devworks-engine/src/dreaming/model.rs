//! Dreaming-specific selection and reporting through the shared native router and Usage owner.
use super::{NativeDreaming, invalid};
use crate::providers::driver::{NativeInvocation, NativeProviders};
use devworks_core::context::InvocationContext;
use devworks_core::ports::storage::{
    Cancellation, DreamCheckpoint, DreamRunRow, ReadOptions, StorageError, StoredAttemptBalance,
    StoredInferenceAttempt, read_snapshot,
};
use devworks_core::ports::usage::{PriceQuery, PriceResolver};
use devworks_core::protocol::provider::{
    ArtifactRef, Attribution, InferenceInput, InferenceOptions, InferencePurpose, InferenceRequest,
    ModelSelection,
};
use devworks_core::protocol::records::{EffortName, RouteIntent, TierName};
use devworks_core::protocol::{Counter, Id, Protocol, Revision};
use devworks_core::services::dreaming::{DreamTools, SweepLease};
use devworks_core::services::prices::CatalogPriceResolver;
use devworks_core::services::router::accounting::{AttemptSelection, ReportCategory};
use devworks_core::services::router::{AuxiliaryRequest, Purpose, RequestAdmission};
use devworks_core::services::usage::accounting::{self, AttemptRequest};
use devworks_core::services::usage::meter::InputSemantics;
use devworks_core::services::usage::pricing::{self, PricingContext};
use devworks_core::services::usage::{auxiliary, host::DriveOutcome};
use serde_json::json;

pub(super) enum RoundGate {
    Ready(Box<NativeInvocation>),
    Blocked,
    Budget,
    Unmeasured,
}

pub(super) struct RoundObservation {
    pub attempt: StoredInferenceAttempt,
    pub balance: StoredAttemptBalance,
    pub result: crate::providers::sink::ProviderResult,
}

impl NativeDreaming {
    pub(super) fn round(
        &self,
        ctx: &InvocationContext,
        lease: &SweepLease,
        run: &DreamRunRow,
        request_id: &Id,
        frame: ArtifactRef,
        elapsed_ms: u64,
    ) -> Result<RoundGate, StorageError> {
        let checkpoint = run.checkpoint.as_ref().cloned().unwrap_or_default();
        if checkpoint.usage_uncertain {
            return Ok(RoundGate::Unmeasured);
        }
        let input_remaining = u64::from(run.policy.max_tokens)
            .saturating_sub(checkpoint.input_tokens)
            .saturating_sub(checkpoint.output_tokens);
        let tools = DreamTools::frozen().aliases();
        let schemas_bytes = tools.iter().try_fold(0u64, |total, tool| {
            let schema = DreamTools::frozen()
                .resolve(tool)
                .map_err(|_| StorageError::ScopeDenied)?;
            let document = serde_json::to_vec(&schema.schema)
                .map_err(|_| invalid("invalid private tool schema"))?;
            Ok::<_, StorageError>(total.saturating_add(document.len() as u64 + 256))
        })?;
        // The source owner also verifies the whole native encoded body before send. Count
        // UTF-8 bytes as an upper bound on prompt tokens, including the tool definitions.
        let reserve_input = u64::try_from(frame.byte_length.get())
            .map_err(|_| invalid("negative prompt length"))?
            .saturating_add(schemas_bytes)
            .saturating_add(16_384);
        let remaining_ms = u64::from(run.policy.max_elapsed_ms).saturating_sub(elapsed_ms);
        if checkpoint.turn >= run.policy.max_turns
            || remaining_ms < 1
            || reserve_input >= input_remaining
        {
            return Ok(RoundGate::Budget);
        }
        let output = input_remaining.saturating_sub(reserve_input).min(8192);
        let remaining_ms = u32::try_from(remaining_ms).unwrap_or(run.policy.max_elapsed_ms);
        let store_info = self.store.info();
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let effort = run.policy.effort.clone().unwrap_or(EffortName::Default);
            let intent = run.policy.model_id.clone().map_or(
                RouteIntent::Tier {
                    tier: TierName::Low,
                },
                |model_id| RouteIntent::Model { model_id },
            );
            let required_capabilities = vec!["tools".to_owned()];
            // An existing conversational pin belongs to its conversation. The sweep uses the
            // explicitly configured model or the current qualified Low model for this purpose.
            let route = self.router.route_auxiliary(
                read,
                &AuxiliaryRequest {
                    purpose: Purpose::Enrichment,
                    session_id: None,
                    intent: intent.clone(),
                    effort: effort.clone(),
                    required_capabilities: required_capabilities.clone(),
                },
            )?;
            let Ok(route) = route else {
                return Ok(RoundGate::Blocked);
            };
            if route.endpoint_profile == "CodexSubscriptionResponses" {
                return Ok(RoundGate::Blocked);
            }
            let owner = match auxiliary::ownership(read, &store_info, ctx) {
                Ok(owner) => owner,
                Err(auxiliary::AuxiliaryError::Unavailable(_)) => return Ok(RoundGate::Blocked),
                Err(auxiliary::AuxiliaryError::Storage(error)) => return Err(error),
            };
            let concrete = route.endpoint_profile != "OpenRouterChat"
                || !devworks_core::ports::router::is_open_router_auto(&route.upstream_model_id);
            let price = PriceQuery {
                catalog_model_id: route.model_id.clone(),
                provider_model_id: concrete.then_some(route.upstream_model_id),
                connection_id: route.connection_id.clone(),
                endpoint_profile: route.endpoint_profile,
                service_tier: None,
                input_tokens: None,
                cache_write_ttl_ms: None,
                processing_region: None,
                required_modalities: vec!["Text".into()],
            };
            let quote = CatalogPriceResolver.quote(read, &price)?;
            let pricing = PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            };
            let (input, output) = (reserve_input as i64, output as i64);
            if let Some(cap) = run.policy.max_micro_usd {
                let estimate = pricing::reservation(&quote, input, output, &pricing, &[])
                    .map_err(|_| invalid("Dreaming reservation could not be computed"))?;
                if !estimate.unknown_components.is_empty()
                    || estimate.tracked_micro_usd.is_none_or(|amount| {
                        checkpoint.priced_micro_usd.saturating_add(amount as u64) > cap
                    })
                {
                    return Ok(RoundGate::Budget);
                }
            }
            let root = ctx
                .root_id()
                .map(|root| read.runs().root_control(root))
                .transpose()?
                .flatten();
            let (generation, cancellation) = root.map_or((1, 1), |row| {
                (row.generation, row.stop_epoch.saturating_add(1))
            });
            let revision = |value| Revision::new(value).map_err(invalid);
            let logical = RequestAdmission {
                request_id: request_id.clone(),
                purpose: Purpose::Enrichment,
                session_id: ctx.session_id().cloned(),
                project_id: ctx.project_id().cloned(),
                root_id: ctx.root_id().cloned(),
                model_id: route.model_id.clone(),
                connection_id: route.connection_id,
                pin_id: None,
                args_digest: String::new(),
            };
            let request = InferenceRequest {
                options: InferenceOptions {
                    selection: ModelSelection::Model {
                        model_id: route.model_id.clone(),
                    },
                    effort: None,
                    output_schema: None,
                    deadline_ms: remaining_ms,
                },
                authority_revision: ctx.policy_revision().clone(),
                control_generation: revision(generation)?,
                instruction_revision: revision(run.cutoffs.base_instruction_revision.max(1))?,
                cancellation_revision: revision(cancellation)?,
                protocol: Protocol::Draft1,
                request_id: request_id.clone(),
                purpose: InferencePurpose::Dreaming,
                attribution: Attribution::Service {
                    operation_id: lease.operation().id.clone(),
                    project_id: ctx.project_id().cloned(),
                },
                input: InferenceInput::Generation {
                    session_id: ctx
                        .session_id()
                        .cloned()
                        .unwrap_or_else(|| lease.operation().id.0.clone()),
                    context: frame,
                    context_epoch: Counter::new(owner.expected_context_epoch.unwrap_or(0))
                        .map_err(invalid)?,
                    tools: tools.clone(),
                    response_headroom_tokens: revision(output)?,
                    requires_images: false,
                    requires_parallel_tools: false,
                },
            };
            let attempt = AttemptRequest {
                admission_key: request_id.clone(),
                request_id: request_id.clone(),
                actor_id: ctx.principal().clone(),
                work_id: owner.work_id,
                job_run_id: owner.job_run_id,
                role: "Dreaming".to_owned(),
                quote,
                price_generation: read.usage().price_revision()?,
                override_revision: read.usage().price_override_revision(&route.model_id)?,
                query: price,
                pricing,
                selection: AttemptSelection::Conversation {
                    intent,
                    effort,
                    required_capabilities,
                    pin: None,
                },
                input_tokens: input,
                output_headroom: output,
                unit_headroom: vec![],
                expected_context_epoch: owner.expected_context_epoch,
                dispatch: owner.dispatch,
                cache_diagnostics: vec![],
            };
            Ok(RoundGate::Ready(Box::new(NativeInvocation {
                source_fences: lease.send_fences(request_id),
                instruction_targets: Some(vec![]),
                context: ctx.clone(),
                logical,
                category: ReportCategory::Other,
                attempt,
                request,
                nested: None,
            })))
        })
    }

    pub(super) fn send(
        &self,
        lease: &SweepLease,
        invocation: NativeInvocation,
        cancel: &Cancellation,
    ) -> Result<DriveOutcome, StorageError> {
        let provider = self
            .providers
            .upgrade()
            .ok_or(StorageError::ViewUnavailable)?;
        self.runtime
            .block_on(provider.produce_dreaming(invocation, cancel.clone(), lease.clone()))
            .map(|outcome| outcome.accounting)
    }

    pub(super) fn recorded_round(
        &self,
        ctx: &InvocationContext,
        request: &Id,
    ) -> Result<Option<RoundObservation>, StorageError> {
        let saved = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let Some(route) = read.router().route_request(request)? else {
                return Ok(None);
            };
            if route.purpose != Purpose::Enrichment.as_str()
                || route.project_id.as_ref() != ctx.project_id()
            {
                return Err(StorageError::ScopeDenied);
            }
            let operation = read
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let Some(journal) = operation.latest_attempt() else {
                return Ok(None);
            };
            let attempt = read
                .usage()
                .inference_attempt(&journal.id.0)?
                .ok_or(StorageError::ScopeDenied)?;
            if attempt.request_id != *request
                || attempt.actor_id != *ctx.principal()
                || attempt.role != "Dreaming"
                || accounting::snapshot(&attempt)?.admission_key != *request
            {
                return Err(StorageError::ScopeDenied);
            }
            let balance = read
                .usage()
                .attempt_balance(&attempt.attempt_id)?
                .ok_or(StorageError::ScopeDenied)?;
            Ok(Some((attempt, balance)))
        })?;
        let Some((attempt, balance)) = saved else {
            return Ok(None);
        };
        let providers = self
            .providers
            .upgrade()
            .ok_or(StorageError::ViewUnavailable)?;
        let result = NativeProviders::recorded_result(&providers, request, request)?;
        Ok(Some(RoundObservation {
            attempt,
            balance,
            result,
        }))
    }
}

impl RoundObservation {
    pub(super) fn meter(&self, checkpoint: &mut DreamCheckpoint) -> Result<(), StorageError> {
        let summary = accounting::summary(&self.balance)?;
        let quantities = summary.meter.quantities;
        let pricing = accounting::snapshot(&self.attempt)?.pricing;
        let count = |amount: Option<i64>| {
            amount
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0)
        };
        let input = quantities
            .total_input(pricing.input_semantics)
            .map_err(|_| invalid("invalid retained Dreaming token count"))?;
        checkpoint.usage_uncertain |= input.is_none() || quantities.output_tokens.is_none();
        checkpoint.input_tokens = checkpoint.input_tokens.saturating_add(count(input));
        checkpoint.output_tokens = checkpoint
            .output_tokens
            .saturating_add(count(quantities.output_tokens));
        checkpoint.priced_micro_usd = checkpoint.priced_micro_usd.saturating_add(
            self.balance
                .priced_micro_usd
                .and_then(|price| u64::try_from(price).ok())
                .unwrap_or(0),
        );
        checkpoint.turn = checkpoint.turn.saturating_add(1);
        Ok(())
    }

    pub(super) fn identity(&self) -> serde_json::Value {
        json!({
            "modelId": self.attempt.catalog_model_id,
            "connectionId": self.attempt.connection_id,
            "attemptId": self.attempt.attempt_id,
            "inputSource": self.result.source_context,
            "monetaryComplete": self.balance.monetary_complete,
        })
    }
}
