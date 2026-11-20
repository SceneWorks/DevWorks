//! Deliver a saved decision through the same provider attempt journal used by generation.
use super::{DecisionProduction, Reply, Source, request};
use crate::dispatch::Consumer;
use crate::effect_consumer::{PanicReasons, STANDARD_RETRY, effect_consumer};
use crate::providers::schemas::RegisteredSchemas;
use crate::usage::UsageRuntime;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::packages::SchemaValidator;
use devworks_core::ports::provider_http::StreamingHttp;
use devworks_core::ports::storage::*;
use devworks_core::ports::usage::{PriceQuery, PriceResolver};
use devworks_core::protocol::provider::SchemaRef;
use devworks_core::protocol::records::{DecisionResult, SchemaValue, UsageReport};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedList, Check, DynamicJson, Money};
use devworks_core::protocol::{Counter, Id, OperationState};
use devworks_core::providers::materialize::{OwnedMaterials, SchemaDocuments};
use devworks_core::providers::systemone;
use devworks_core::services::artifacts::{ArtifactService, host::HostArtifactPublication};
use devworks_core::services::effect::{EffectOutcome, JournaledEffect};
use devworks_core::services::prices::CatalogPriceResolver;
use devworks_core::services::router::accounting::{AttemptSelection, ReportCategory};
use devworks_core::services::router::connections::{AccessCredential, ConnectionService};
use devworks_core::services::router::decisions::{
    CONSUMER, DecisionIntent, DecisionService, OPERATION,
};
use devworks_core::services::router::{InferenceRouter, Purpose, RequestAdmission};
use devworks_core::services::usage::{
    accounting::{self, AttemptRequest},
    auxiliary,
    host::DriveOutcome,
    meter::InputSemantics,
    pricing::PricingContext,
};
use devworks_core::services::web::{DecisionMedia, WebService};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(crate) struct SystemOne {
    pub store: Arc<dyn ApplicationStore>,
    pub artifacts: Arc<ArtifactService>,
    pub router: Arc<InferenceRouter>,
    pub decisions: Arc<DecisionService>,
    pub connections: Arc<ConnectionService>,
    pub web: Option<Arc<WebService>>,
    pub authority: Arc<dyn devworks_core::ports::AuthorityPort>,
    pub http: Arc<dyn StreamingHttp>,
    pub validator: Arc<dyn SchemaValidator>,
    pub usage: Arc<UsageRuntime>,
    pub schemas: OnceLock<Arc<RegisteredSchemas>>,
    pub runtime: tokio::runtime::Handle,
}

impl SystemOne {
    pub fn consumer(self: &Arc<Self>) -> Consumer {
        effect_consumer(
            CONSUMER,
            self.clone(),
            PanicReasons {
                execute: "SystemOne decision delivery ended without an outcome",
                observe: "SystemOne decision reconciliation ended without an outcome",
            },
            STANDARD_RETRY,
            Some(4),
        )
    }

    fn execute_owned(
        &self,
        operation: &OperationRecord,
        cancel: &Cancellation,
    ) -> Result<EffectOutcome, StorageError> {
        let intent = intent(operation)?;
        let info = self.store.info();
        let context = intent.source_context(&info)?;
        let schemas = self.schemas.get().ok_or(StorageError::ViewUnavailable)?;
        let author = devworks_core::ports::Authorization::direct()
            .fenced_by(intent.authority_fences.clone());
        let owned = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            if !devworks_core::authority::service::authorization_holds(read, &author)? {
                return Err(StorageError::ScopeDenied);
            }
            auxiliary::ownership(read, &info, &context).map_err(|error| match error {
                auxiliary::AuxiliaryError::Storage(error) => error,
                auxiliary::AuxiliaryError::Unavailable(_) => StorageError::ScopeDenied,
            })
        })?;
        if cancel.is_cancelled()
            || read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                Ok(read
                    .queue()
                    .entry(&intent.delivery_id)?
                    .is_none_or(|entry| entry.cancel_requested))
            })?
        {
            return Ok(EffectOutcome::Failed(
                "SystemOne decision cancelled before dispatch".into(),
            ));
        }
        let deadline = Instant::now()
            + Duration::from_millis(
                intent
                    .input
                    .timeout_ms
                    .0
                    .parse()
                    .map_err(|_| StorageError::ScopeDenied)?,
            );
        let material = OwnedMaterials {
            store: &*self.store,
            artifacts: &*self.artifacts,
            context: &context,
            schemas: &**schemas,
        };
        let mut reason = "No saved decision model still meets the request requirements";
        let pooled = intent.input.use_pool == Some(true);
        let mut remote = |url: &str, video, declared: Option<&str>, remaining_bytes| {
            self.web
                .as_ref()
                .ok_or(devworks_core::providers::ProviderError::MaterialUnavailable)?
                .decision_media(
                    &context,
                    &*self.authority,
                    DecisionMedia {
                        url,
                        video,
                        declared,
                        deadline,
                        cancellation: cancel,
                        remaining_bytes,
                    },
                )
        };
        let materialized =
            match systemone::encode(&intent.input, "", &material, &*self.validator, &mut remote) {
                Ok(body) => body,
                Err(_) if cancel.is_cancelled() => {
                    return Ok(EffectOutcome::Failed(
                        "SystemOne decision cancelled before dispatch".into(),
                    ));
                }
                Err(_) => {
                    return Ok(EffectOutcome::Failed(
                        "SystemOne media or question material could not be prepared".into(),
                    ));
                }
            };
        let models = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            intent
                .candidates
                .iter()
                .map(|candidate| {
                    self.decisions
                        .current_candidate(
                            read.router(),
                            candidate,
                            &intent.requirements,
                            pooled,
                            read.stamp().now_ms,
                        )
                        .map(|model| model.map(|model| (candidate.clone(), model)))
                })
                .collect::<Result<Vec<_>, _>>()
        })?;
        let mut verified = Vec::new();
        for (candidate, model) in models.into_iter().flatten() {
            let schema: SchemaRef = serde_json::from_str(&model.result_schema).map_err(invalid)?;
            let upstream_bytes = serde_json::to_vec(&model.upstream_model_id)
                .map_err(invalid)?
                .len();
            let input_bytes = materialized
                .len()
                .saturating_sub(2)
                .saturating_add(upstream_bytes);
            if check_body_size(&model, &intent, input_bytes)?
                && input_bytes <= systemone::MAX_REQUEST_BYTES
                && schema_document(schemas.as_ref(), &context, &schema).is_ok()
                && self.connections.access(&model.connection_id, None).is_ok()
            {
                verified.push(candidate);
            }
        }
        let candidates = {
            let service = self.decisions.clone();
            let operation = operation.clone();
            let intent = intent.clone();
            let input_digest = devworks_core::fingerprint::sha256_hex(&materialized);
            self.usage
                .accounting_fence()
                .write(&*self.store, move |tx| {
                    service.allocate(tx, &operation, &intent, verified, &input_digest)
                })?
        };
        let mut materialized: Value = serde_json::from_slice(&materialized).map_err(invalid)?;
        for candidate in &candidates {
            if cancel.is_cancelled() {
                return Ok(EffectOutcome::Failed("SystemOne decision cancelled".into()));
            }
            let prepared = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                if !devworks_core::authority::service::authorization_holds(read, &author)? {
                    return Err(StorageError::ScopeDenied);
                }
                let Some(model) = self.decisions.current_candidate(
                    read.router(),
                    candidate,
                    &intent.requirements,
                    pooled,
                    read.stamp().now_ms,
                )?
                else {
                    return Ok(None);
                };
                let connection = read
                    .router()
                    .connection(&model.connection_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                let mut modalities = vec!["Text".into()];
                if intent.requirements.images {
                    modalities.push("Image".into());
                }
                if intent.requirements.videos {
                    modalities.push("Video".into());
                }
                let query = PriceQuery {
                    catalog_model_id: model.id.clone(),
                    provider_model_id: Some(model.upstream_model_id.clone()),
                    connection_id: model.connection_id.clone(),
                    endpoint_profile: "SystemOne".into(),
                    service_tier: None,
                    input_tokens: None,
                    cache_write_ttl_ms: None,
                    processing_region: None,
                    required_modalities: modalities,
                };
                Ok(Some((model, connection, query)))
            })?;
            let Some((model, connection, mut query)) = prepared else {
                continue;
            };
            let result_schema: SchemaRef =
                serde_json::from_str(&model.result_schema).map_err(invalid)?;
            let schema = match schema_document(schemas.as_ref(), &context, &result_schema) {
                Ok(schema) => schema,
                Err(_) => {
                    reason = "Decision result schema is unavailable or changed";
                    continue;
                }
            };
            materialized["model"] = json!(&model.upstream_model_id);
            let body = serde_json::to_vec(&materialized).map_err(invalid)?;
            if !check_body_size(&model, &intent, body.len())? {
                reason = "Decision input exceeds this model's configured limits";
                continue;
            }
            let batch_bytes = body
                .len()
                .saturating_mul(intent.requirements.question_count);
            query.input_tokens = Some(batch_bytes as i64);
            let (quote, price_generation) =
                read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    Ok((
                        CatalogPriceResolver.quote(read, &query)?,
                        read.usage().price_revision()?,
                    ))
                })?;
            if quote.connection_revision != Some(connection.revision) {
                continue;
            }
            let access = match self.connections.access(&model.connection_id, None) {
                Ok(access)
                    if access.profile == "SystemOne"
                        && access.endpoint == connection.endpoint
                        && access.credential_revision == connection.credential_revision =>
                {
                    access
                }
                _ => {
                    reason = "Decision connection credentials are unavailable or changed";
                    continue;
                }
            };
            let token = match &access.credential {
                AccessCredential::None => None,
                AccessCredential::ApiKey(key) => Some(key),
                AccessCredential::Bearer { .. } => return Err(StorageError::ScopeDenied),
            };
            let timeout = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            if timeout == 0 {
                return Ok(EffectOutcome::Failed(
                    "SystemOne decision deadline expired before dispatch".into(),
                ));
            }
            let request_id = candidate_request(operation, &model.id)?;
            let admission_key = request_id.clone();
            let logical = RequestAdmission {
                request_id: request_id.clone(),
                purpose: Purpose::Decision,
                session_id: context.session_id().cloned(),
                project_id: context.project_id().cloned(),
                root_id: context.root_id().cloned(),
                model_id: model.id.clone(),
                connection_id: model.connection_id.clone(),
                pin_id: None,
                args_digest: devworks_core::fingerprint::sha256_hex(&body),
            };
            let reply = Arc::new(Mutex::new(None));
            let source = Source::new(
                self.http.clone(),
                request(&access.endpoint, token, body),
                timeout,
                reply.clone(),
            );
            let attempt = AttemptRequest {
                admission_key,
                request_id: request_id.clone(),
                actor_id: context.principal().clone(),
                work_id: owned.work_id.clone(),
                job_run_id: owned.job_run_id.clone(),
                role: owned.role.clone(),
                override_revision: quote.override_revision.unwrap_or_default(),
                price_generation,
                query,
                quote,
                pricing: PricingContext {
                    input_semantics: InputSemantics::Disjoint,
                    cache_read_possible: true,
                    cache_write_possible: true,
                    cache_write_ttl_ms: None,
                },
                selection: AttemptSelection::Decision {
                    model_revision: model.revision,
                    connection_revision: connection.revision,
                    account_revision: connection.account_revision,
                    credential_revision: connection.credential_revision,
                    endpoint_digest: devworks_core::fingerprint::sha256_hex(
                        connection.endpoint.as_bytes(),
                    ),
                    requirements: intent.requirements.clone(),
                    pooled,
                },
                input_tokens: batch_bytes as i64,
                output_headroom: systemone::MAX_RESPONSE_BYTES as i64,
                unit_headroom: Vec::new(),
                expected_context_epoch: owned.expected_context_epoch,
                dispatch: owned.dispatch.clone(),
                cache_diagnostics: Vec::new(),
            };
            let category = match context.principal_kind() {
                PrincipalKind::McpClient => ReportCategory::Mcp,
                PrincipalKind::Agent => ReportCategory::for_agent_role(&owned.role),
                _ => ReportCategory::Other,
            };
            let outcome =
                self.runtime
                    .block_on(self.usage.produce_decision(DecisionProduction {
                        context: context.clone(),
                        logical,
                        request: attempt,
                        category,
                        source_fences: intent.authority_fences.clone(),
                        source: Box::new(source),
                        cancellation: cancel.clone(),
                    }))?;
            let result = reply
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take();
            match (outcome, result) {
                (DriveOutcome::Settled(balance), Some(Reply::Read { raw, value }))
                    if !balance.unknown_tail =>
                {
                    let value: DynamicJson = match serde_json::from_value(value) {
                        Ok(value) => value,
                        Err(_) => {
                            reason = "Decision output exceeds the bounded result contract";
                            continue;
                        }
                    };
                    if self.validator.validate(&schema, &value.0).is_err()
                        || !matches_questions(&intent, &value.0)
                    {
                        let operation = operation.id.clone();
                        let model_id = model.id.clone();
                        let digest = devworks_core::fingerprint::sha256_hex(&raw);
                        self.usage.accounting_fence().write(&*self.store, move |tx| {
                            let record = tx.operations().operation(&operation)?.ok_or(StorageError::ScopeDenied)?;
                            let attempt = record.latest_attempt().ok_or(StorageError::ScopeDenied)?;
                            tx.operations().observe(&operation, attempt.attempt_no, json!({
                                "decisionResponseRejected": {"modelId":model_id,"responseDigest":digest,"reason":"SchemaMismatch"}
                            }))
                        })?;
                        reason = "Decision endpoint returned a result that does not match the requested questions or schema";
                        continue;
                    }
                    let result = self.publish(
                        operation,
                        &intent,
                        &context,
                        CompletedDecision {
                            request_id,
                            model_id: model.id,
                            schema: result_schema,
                            value,
                            raw,
                        },
                        cancel,
                    )?;
                    return Ok(EffectOutcome::Completed(json!({"result":result})));
                }
                (
                    DriveOutcome::Settled(balance),
                    Some(Reply::Refused {
                        status,
                        retry_after_seconds,
                    }),
                ) if !balance.unknown_tail => {
                    self.note_refusal(model, connection, status, retry_after_seconds)?;
                    reason = match status {
                        401 | 403 => "Decision endpoint refused authentication",
                        402 => "Decision endpoint refused billing or credits",
                        429 | 503 => "Decision endpoint refused capacity",
                        400 | 422 => "Decision endpoint rejected the request input",
                        _ => "Decision endpoint refused the request",
                    };
                    if !matches!(status, 401 | 402 | 403 | 408 | 429 | 500..=599) {
                        break;
                    }
                }
                (DriveOutcome::Settled(balance), Some(Reply::NotSent)) if !balance.unknown_tail => {
                    reason = "Decision endpoint was unreachable before send"
                }
                (DriveOutcome::Settled(balance), Some(Reply::Malformed))
                    if !balance.unknown_tail =>
                {
                    reason = "Decision endpoint returned invalid JSON"
                }
                (DriveOutcome::Refused(accounting::Admission::SourceChanged(_)), _) => {
                    reason = "Decision route changed before dispatch"
                }
                (DriveOutcome::Refused(accounting::Admission::Blocked(_)), _) => {
                    return Ok(EffectOutcome::Failed(
                        "Feature budget refused the decision request".into(),
                    ));
                }
                (DriveOutcome::Refused(_), _) => {
                    return Ok(EffectOutcome::Failed(
                        "Decision work or session authority changed".into(),
                    ));
                }
                (DriveOutcome::CancelledBeforeDispatch, _) => {
                    return Ok(EffectOutcome::Failed(
                        "Decision was cancelled before dispatch".into(),
                    ));
                }
                _ => {
                    return Ok(EffectOutcome::Unknown(
                        "Decision transport did not establish a complete outcome".into(),
                    ));
                }
            }
        }
        Ok(EffectOutcome::Failed(reason.into()))
    }

    fn note_refusal(
        &self,
        model: StoredDecisionModel,
        connection: StoredConnection,
        status: u16,
        after: Option<u64>,
    ) -> Result<(), StorageError> {
        let reason = match status {
            401 | 403 => {
                return self
                    .usage
                    .accounting_fence()
                    .write(&*self.store, move |tx| {
                        devworks_core::services::router::connections::record_systemone_rejection(
                            tx,
                            &connection,
                            status,
                        )
                        .map(|_| ())
                    });
            }
            402 => "PlanRejected",
            429 => "RateLimited",
            503 => "Overloaded",
            _ => return Ok(()),
        };
        let delay_ms = after.unwrap_or(60).clamp(1, 86_400) as i64 * 1000;
        let router = self.router.clone();
        self.usage
            .accounting_fence()
            .write(&*self.store, move |tx| {
                if !tx
                    .router()
                    .decision_model(&model.id)?
                    .is_some_and(|current| current.revision == model.revision)
                    || !tx
                        .router()
                        .connection(&connection.id)?
                        .is_some_and(|current| current.revision == connection.revision)
                {
                    return Ok(());
                }
                let until = tx.now_ms().saturating_add(delay_ms);
                router
                    .record_cooldown(tx, &model.id, until, reason)
                    .map(|_| ())
            })
    }

    fn publish(
        &self,
        operation: &OperationRecord,
        intent: &DecisionIntent,
        context: &InvocationContext,
        complete: CompletedDecision,
        cancel: &Cancellation,
    ) -> Result<OperationResult, StorageError> {
        let fence = self.usage.accounting_fence();
        fence.publication(|| {
            if cancel.is_cancelled() { return Err(StorageError::Cancelled); }
            let redactor = devworks_core::authority::redact::Redactor::new();
            let redacted = redactor.redact_json(&complete.value.0);
            let raw = if redacted == complete.value.0 { complete.raw } else { serde_json::to_vec(&redacted).map_err(invalid)? };
            let staged = self.artifacts.stage_host_bytes(&*self.store, &raw, json!({"kind":"SystemOneDecision", "operationId":operation.id}))?
                .map_err(|_| StorageError::ViewUnavailable)?;
            let store = &*self.store;
            let service = self.artifacts.clone();
            let pending = staged.clone();
            let operation = operation.clone();
            let intent = intent.clone();
            let context = context.clone();
            let cancel = cancel.clone();
            let committed = run_write(store, WriteOptions::observed(), move |tx| {
                if cancel.is_cancelled() || !devworks_core::authority::service::fences_hold(tx, &intent.authority_fences)? {
                    return Err(StorageError::ScopeDenied);
                }
                let current = tx.operations().operation(&operation.id)?.ok_or(StorageError::ScopeDenied)?;
                let attempt = current.latest_attempt().ok_or(StorageError::ScopeDenied)?;
                if current.state != OperationState::Dispatching || Some(attempt.fence()) != operation.latest_attempt().map(AttemptRecord::fence) {
                    return Err(StorageError::ScopeDenied);
                }
                let delivery = tx.queue().entry(&intent.delivery_id)?.ok_or(StorageError::ScopeDenied)?;
                if delivery.cancel_requested || delivery.operation_id.as_ref() != Some(&operation.id) {
                    return Err(StorageError::Cancelled);
                }
                let version = service.commit_host_bytes_in_scope(tx, &pending, HostArtifactPublication {
                    title: "SystemOne response", media_type: "application/json",
                    provenance: json!({"kind":"SystemOneDecision", "trust":"Untrusted", "requestId":complete.request_id, "operationId":operation.id}),
                    scope: context.project_id().cloned().map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                    owner: context.principal().clone(),
                })?;
                let result = OperationResult::RouterDecide { value: DecisionResult {
                    model_id: complete.model_id, request_id: complete.request_id,
                    output: SchemaValue { schema: complete.schema, value: DynamicJson(redacted) },
                    raw_response: devworks_core::services::artifacts::version_ref(&version),
                    usage: usage_report(tx, &operation, &intent)?,
                }};
                result.check().map_err(StorageError::InvalidRecord)?;
                tx.operations().observe(&operation.id, attempt.attempt_no, json!({"decisionResult":result}))?;
                Ok(result)
            });
            self.artifacts.settle_host_bytes(store, &staged, committed.is_ok());
            committed
        })
    }
}

struct CompletedDecision {
    request_id: Id,
    model_id: Id,
    schema: SchemaRef,
    value: DynamicJson,
    raw: Vec<u8>,
}

fn candidate_request(operation: &OperationRecord, model: &Id) -> Result<Id, StorageError> {
    Id::new(format!(
        "systemone-{}",
        devworks_core::fingerprint::sha256_hex(
            format!("{}\n{}", operation.id.0.as_str(), model.as_str()).as_bytes()
        )
    ))
    .map_err(|_| StorageError::ScopeDenied)
}

fn count(value: i64) -> Counter {
    Counter::new(value.max(0)).expect("nonnegative")
}

fn usage_report(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    intent: &DecisionIntent,
) -> Result<UsageReport, StorageError> {
    let mut tracked = 0_i64;
    let mut untracked = 0;
    let mut incomplete = 0;
    let mut items = Vec::new();
    for candidate in &intent.candidates {
        let id = candidate_request(operation, &candidate.model_id)?;
        let Some(route) = tx.router().route_request(&id)? else {
            continue;
        };
        let journal = tx
            .operations()
            .operation(&route.operation_id)?
            .ok_or_else(|| {
                StorageError::InvalidRecord("decision attempt journal missing".into())
            })?;
        let read = tx.usage();
        for attempt in journal.attempts {
            let Some(balance) = read.attempt_balance(&attempt.id.0)? else {
                continue;
            };
            let note = accounting::summary(&balance)?;
            tracked = tracked.saturating_add(balance.priced_micro_usd.unwrap_or(0));
            untracked += i64::from(balance.priced_micro_usd.is_none());
            incomplete += i64::from(balance.incomplete || !balance.monetary_complete);
            let quantities = note.meter.quantities;
            let (Some(input), Some(output), Some(cache_read), Some(cache_write)) = (
                quantities.input_tokens,
                quantities.output_tokens,
                quantities.cached_read_tokens,
                quantities.cache_write_tokens,
            ) else {
                continue;
            };
            let (kind, origin) = balance.classification.price();
            items.push(devworks_core::protocol::records::UsageItem {
                request_id: id.clone(),
                purpose: devworks_core::protocol::support::BoundedText("Decision".into()),
                model_id: candidate.model_id.clone(),
                input_tokens: count(input),
                output_tokens: count(output),
                cache_read_tokens: count(cache_read),
                cache_write_tokens: count(cache_write),
                amount_usd: balance.priced_micro_usd.map(money),
                price_kind: serde_json::from_value(json!(kind)).map_err(invalid)?,
                price_origin: serde_json::from_value(json!(origin)).map_err(invalid)?,
                price_revision: None,
                incomplete: balance.incomplete || !balance.monetary_complete,
            });
        }
    }
    Ok(UsageReport {
        items: BoundedList(items),
        total_tracked_usd: money(tracked),
        untracked_requests: count(untracked),
        incomplete_requests: count(incomplete),
        next_cursor: None,
    })
}

fn money(amount: i64) -> Money {
    if amount == 0 {
        return Money("0".into());
    }
    Money(format!("{}.{:06}", amount / 1_000_000, amount % 1_000_000))
}

fn check_body_size(
    model: &StoredDecisionModel,
    intent: &DecisionIntent,
    actual: usize,
) -> Result<bool, StorageError> {
    let Some(policy) = &model.decision_policy else {
        return Ok(true);
    };
    let policy: devworks_core::protocol::records::DecisionPolicy =
        serde_json::from_str(policy).map_err(invalid)?;
    let fits = |actual, limit: &str| limit.parse::<usize>().is_ok_and(|limit| actual <= limit);
    let batch = intent.requirements.batch_bytes.saturating_add(
        actual
            .saturating_sub(intent.requirements.bytes)
            .saturating_mul(intent.requirements.question_count),
    );
    Ok(fits(actual, &policy.max_input_bytes.0) && fits(batch, &policy.max_batch_bytes.0))
}

fn matches_questions(intent: &DecisionIntent, value: &Value) -> bool {
    let Some(answers) = value["answers"].as_object() else {
        return false;
    };
    if answers.len() != intent.input.questions.0.len() {
        return false;
    }
    intent.input.questions.0.iter().all(|entry| {
        use devworks_core::protocol::records::DecisionQuestion;
        let kind = match &entry.question {
            DecisionQuestion::Choice { .. } => "choice",
            DecisionQuestion::Noul { .. } => "noul",
            DecisionQuestion::Score { .. } => "score",
            DecisionQuestion::Box { .. } => "box",
            DecisionQuestion::Extension { payload, .. } => {
                payload.0["type"].as_str().unwrap_or_default()
            }
        };
        answers.get(&entry.name.0).is_some_and(|answer| {
            if answer["type"] != kind {
                return false;
            }
            let probability = |value: &Value| {
                value
                    .as_f64()
                    .is_some_and(|number| (0.0..=1.0).contains(&number))
            };
            match &entry.question {
                DecisionQuestion::Choice { criteria, .. } => {
                    let chosen = answer["choice"].as_str();
                    criteria
                        .0
                        .iter()
                        .any(|criterion| Some(criterion.name.0.as_str()) == chosen)
                        && answer["probabilities"].as_object().is_some_and(|values| {
                            values.len() == criteria.0.len()
                                && criteria.0.iter().all(|criterion| {
                                    values.get(&criterion.name.0).is_some_and(probability)
                                })
                        })
                        && probability(&answer["confidence"])
                }
                DecisionQuestion::Noul { .. } => probability(&answer["noul"]),
                DecisionQuestion::Score { criteria, .. } => {
                    answer["score"].as_f64().is_some_and(|score| {
                        (0.0..=criteria.0.len().saturating_sub(1) as f64).contains(&score)
                    }) && probability(&answer["confidence"])
                        && answer["legend"]
                            .as_object()
                            .is_some_and(|legend| legend.len() == criteria.0.len())
                        && answer["probabilities"]
                            .as_object()
                            .is_some_and(|values| values.len() == criteria.0.len())
                        && criteria.0.iter().enumerate().all(|(index, label)| {
                            let level = index.to_string();
                            answer["legend"][&level] == label.0
                                && probability(&answer["probabilities"][&level])
                        })
                }
                DecisionQuestion::Box { .. } => {
                    let edges = &answer["box"];
                    ["top", "bottom", "left", "right"]
                        .into_iter()
                        .all(|edge| probability(&edges[edge]))
                        && edges["top"].as_f64() <= edges["bottom"].as_f64()
                        && edges["left"].as_f64() <= edges["right"].as_f64()
                        && answer
                            .get("confidence")
                            .is_some_and(|value| value.is_null() || probability(value))
                }
                DecisionQuestion::Extension { .. } => {
                    answer.as_object().is_some_and(|value| value.len() > 1)
                }
            }
        })
    })
}

fn schema_document(
    schemas: &RegisteredSchemas,
    context: &InvocationContext,
    reference: &SchemaRef,
) -> Result<Value, devworks_core::providers::ProviderError> {
    let schema = schemas.resolve(context, reference)?;
    let canonical = devworks_core::jcs::canonicalize(&schema)
        .map_err(|_| devworks_core::providers::ProviderError::InvalidInput("decision_schema"))?;
    if devworks_core::fingerprint::sha256_hex(canonical.as_bytes()) != reference.sha256.as_str() {
        return Err(devworks_core::providers::ProviderError::InvalidInput(
            "decision_schema_digest",
        ));
    }
    Ok(schema)
}

impl JournaledEffect for SystemOne {
    fn observes_delivery_cancellation(&self) -> bool {
        true
    }
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::new())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> EffectOutcome {
        match self.execute_owned(operation, cancellation) {
            Ok(outcome) => outcome,
            Err(StorageError::ScopeDenied) => {
                EffectOutcome::Failed("SystemOne decision authority changed".into())
            }
            Err(StorageError::Cancelled) if self.proves_no_send(operation).unwrap_or(false) => {
                EffectOutcome::Failed("SystemOne decision cancelled before dispatch".into())
            }
            Err(_) => EffectOutcome::Unknown("SystemOne decision needs reconciliation".into()),
        }
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        match observed_result(operation) {
            Some(result) => EffectOutcome::Completed(json!({"result":result})),
            None if self.proves_no_send(operation).unwrap_or(false) => EffectOutcome::NotApplied(
                "No native decision attempt reached a send boundary".into(),
            ),
            None => EffectOutcome::Unknown(
                "SystemOne has no durable decision result for this request".into(),
            ),
        }
    }
}

impl SystemOne {
    fn proves_no_send(&self, operation: &OperationRecord) -> Result<bool, StorageError> {
        let intent = intent(operation)?;
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            for candidate in &intent.candidates {
                if read
                    .router()
                    .route_request(&candidate_request(operation, &candidate.model_id)?)?
                    .is_some()
                {
                    // The ledger already has an attempt identity. Its provider outcome may be
                    // absent or uncertain, so do not classify it from the outer journal alone.
                    return Ok(false);
                }
            }
            Ok(true)
        })
    }
}

pub(super) fn observed_result(operation: &OperationRecord) -> Option<OperationResult> {
    operation
        .attempts
        .iter()
        .rev()
        .flat_map(|attempt| attempt.observations.iter().rev())
        .find_map(|note| {
            let result =
                serde_json::from_value::<OperationResult>(note.get("decisionResult")?.clone())
                    .ok()?;
            (result.operation() == OPERATION).then_some(result)
        })
}

pub(crate) fn completion(
    store: &dyn ApplicationStore,
    context: &InvocationContext,
    id: &devworks_core::protocol::OperationId,
) -> Result<
    Option<devworks_core::protocol::results::OperationCompletion>,
    Box<devworks_core::protocol::DomainFault>,
> {
    use devworks_core::kernel::failure::Refusal;
    use devworks_core::protocol::results::OperationCompletion;
    use devworks_core::protocol::{CorrelationId, DomainErrorCode, Protocol, Revision};
    let correlation = || CorrelationId(id.0.clone());
    let not_found = || {
        Box::new(
            Refusal::new(
                DomainErrorCode::NotFound,
                "systemone_decision",
                Some(id.0.clone()),
            )
            .fault(correlation()),
        )
    };
    if store.info().store_id != *context.store_id()
        || store.info().restore_epoch != context.restore_epoch().get()
    {
        return Err(not_found());
    }
    let operation = read_snapshot(store, &ReadOptions::default(), |read| {
        let Some(operation) = read.operations().operation(id)? else {
            return Ok(None);
        };
        let Ok(saved) = intent(&operation) else {
            return Ok(None);
        };
        let source = saved.source_context(&store.info())?;
        let author =
            devworks_core::ports::Authorization::direct().fenced_by(saved.authority_fences);
        Ok((source.principal() == context.principal()
            && source.project_id() == context.project_id()
            && source.session_id() == context.session_id()
            && source.root_id() == context.root_id()
            && source.job_run_id() == context.job_run_id()
            && source.ingress_namespace() == context.ingress_namespace()
            && devworks_core::authority::service::authorization_holds(read, &author)?)
        .then_some(operation))
    })
    .map_err(|_| not_found())?
    .ok_or_else(not_found)?;
    let revision = Revision::new(operation.revision).map_err(|_| not_found())?;
    let operation_id = operation.id.clone();
    let protocol = Protocol::Draft1;
    let unknown = || OperationCompletion::OutcomeUnknown {
        operation_id: operation_id.clone(),
        protocol: Protocol::Draft1,
        revision: revision.clone(),
        correlation_id: correlation(),
    };
    Ok(match operation.state {
        OperationState::ObservedComplete => Some(match observed_result(&operation) {
            Some(result) => OperationCompletion::Succeeded {
                protocol,
                operation_id: operation_id.clone(),
                revision: revision.clone(),
                result,
            },
            None => unknown(),
        }),
        OperationState::OutcomeUnknown | OperationState::NeedsInput => Some(unknown()),
        OperationState::Failed | OperationState::CancelledBeforeDispatch => {
            let cause = operation.latest_attempt().and_then(|attempt| {
                attempt
                    .observations
                    .iter()
                    .rev()
                    .find_map(|note| note.pointer("/effect/cause").and_then(Value::as_str))
            });
            let (code, retry) = match cause {
                _ if operation.state == OperationState::CancelledBeforeDispatch => (
                    DomainErrorCode::Cancelled,
                    devworks_core::protocol::RetryAdvice::Never,
                ),
                Some(
                    "SystemOne decision cancelled before dispatch"
                    | "Decision was cancelled before dispatch"
                    | "SystemOne decision cancelled",
                ) => (
                    DomainErrorCode::Cancelled,
                    devworks_core::protocol::RetryAdvice::Never,
                ),
                Some("Feature budget refused the decision request") => (
                    DomainErrorCode::BudgetHold,
                    devworks_core::protocol::RetryAdvice::AfterBlockerClears,
                ),
                Some(
                    "Decision endpoint refused authentication"
                    | "Decision connection credentials are unavailable or changed",
                ) => (
                    DomainErrorCode::CredentialsRequired,
                    devworks_core::protocol::RetryAdvice::AfterBlockerClears,
                ),
                Some("Decision endpoint refused capacity") => (
                    DomainErrorCode::CapacityWait,
                    devworks_core::protocol::RetryAdvice::AfterBlockerClears,
                ),
                Some("Decision endpoint rejected the request input") => (
                    DomainErrorCode::InvalidArgument,
                    devworks_core::protocol::RetryAdvice::RefreshAndNewRequest,
                ),
                Some(
                    "Decision endpoint returned a result that does not match the requested questions or schema"
                    | "Decision result schema is unavailable or changed",
                ) => (
                    DomainErrorCode::SchemaMismatch,
                    devworks_core::protocol::RetryAdvice::Never,
                ),
                Some("SystemOne decision authority changed") => (
                    DomainErrorCode::PermissionDenied,
                    devworks_core::protocol::RetryAdvice::Never,
                ),
                _ => (
                    DomainErrorCode::ExternalFailure,
                    devworks_core::protocol::RetryAdvice::Never,
                ),
            };
            Some(OperationCompletion::Failed {
                protocol,
                operation_id: operation_id.clone(),
                revision: revision.clone(),
                fault: Refusal::new(code, "systemone_decision", Some(id.0.clone()))
                    .retry(retry)
                    .fault(correlation()),
            })
        }
        _ => None,
    })
}

fn intent(operation: &OperationRecord) -> Result<DecisionIntent, StorageError> {
    if operation.capability != OPERATION {
        return Err(StorageError::ScopeDenied);
    }
    serde_json::from_value(operation.payload.clone()).map_err(invalid)
}

fn invalid(_: serde_json::Error) -> StorageError {
    StorageError::InvalidRecord("stored SystemOne decision data".into())
}
