//! Native first-generation routing uses the assigned source for both classifier transports.
use crate::providers::driver::NativeInvocation;
use crate::providers::sink::ProviderResult;
use crate::turn_engine::TurnEngine;
use crate::turn_error::TurnError;
use devworks_core::context::InvocationContext;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::operations::router::DecideInput;
use devworks_core::ports::storage::*;
use devworks_core::ports::usage::{PriceQuery, PriceResolver};
use devworks_core::protocol::provider::*;
use devworks_core::protocol::records::{
    DecisionCriterion, DecisionQuestion, EffortName, MediaRef, NamedQuestion, RouteIntent, TierName,
};
use devworks_core::protocol::results::{OperationCompletion, OperationResult};
use devworks_core::protocol::support::{BoundedList, BoundedText, DynamicJson, PositiveCounter};
use devworks_core::protocol::{Counter, DomainErrorCode, Id, Protocol, RetryAdvice, Revision};
use devworks_core::providers::context::{
    ContentBlock, GenerationParameters, Message, MessageRole, ProviderContext, TextBlock,
};
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_core::services::prices::CatalogPriceResolver;
use devworks_core::services::router::auto_routing::Source;
use devworks_core::services::router::{
    Purpose, RequestAdmission,
    accounting::{AttemptSelection, ReportCategory},
    auto_routing::{
        self, Assignment, CLASSIFIER_INSTRUCTIONS, ClassifierKind, LowCandidate, RUBRIC,
        RUBRIC_VERSION,
    },
};
use devworks_core::services::usage::{
    accounting::{Admission, AttemptEnd, AttemptRequest},
    auxiliary,
    host::DriveOutcome,
    meter::InputSemantics,
    pricing::PricingContext,
};
use serde_json::{Value, json};
use std::time::Duration;

const OUTPUT_TOKENS: i64 = 16;

/// Recover the requested route before pinning. A previously chosen tier also survives removal
/// of the initial default. An explicit pin takes precedence until it is deliberately changed.
pub(crate) fn unpinned_selection(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    session: &Id,
) -> Result<(RouteIntent, EffortName), StorageError> {
    if let Some(route) = ctx
        .workspace_reply()
        .and_then(|reply| reply.preference.clone())
    {
        let route: ConversationRoutePreference = serde_json::from_value(route)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        return Ok((route.intent, route.effort));
    }
    let existing = read.router().auto_route(session)?.is_some();
    let managed_auto = if ctx.root_id().is_some() {
        let descriptor =
            devworks_core::settings::catalog::descriptor(auto_routing::MANAGED_AUTO_KEY)
                .expect("known routing setting");
        let layers = devworks_core::settings::model::current_chain(
            read.configuration(),
            &ConfigScope::Workspace,
        )?;
        devworks_core::settings::model::chain(descriptor, &layers)
            .map_err(|_| invalid("managed routing setting schema"))?
            .first()
            .is_some_and(|value| value.value.value.0.as_bool() == Some(true))
    } else {
        false
    };
    let intent = if existing || managed_auto {
        RouteIntent::DevWorksAuto {}
    } else {
        RouteIntent::Tier {
            tier: TierName::High,
        }
    };
    Ok((intent, EffortName::Default))
}

pub(crate) fn needs_assignment(
    engine: &TurnEngine,
    ctx: &InvocationContext,
) -> Result<bool, StorageError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        if read.router().session_pin(session)?.is_some()
            || read
                .router()
                .auto_route(session)?
                .is_some_and(|entry| entry.decision.is_some())
        {
            return Ok(false);
        }
        Ok(matches!(
            unpinned_selection(read, ctx, session)?.0,
            RouteIntent::DevWorksAuto {}
        ))
    })
}

pub(crate) async fn assign(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    origin: String,
    mut material: ProviderContext,
    cancel: &Cancellation,
) -> Result<(), TurnError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    check_cancel(cancel)?;

    let owner_context = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        let owner = auxiliary::ownership(read, &engine.store().info(), ctx)
            .map_err(|_| StorageError::ScopeDenied)?;
        let mut context = format!("Assigned role: {}", owner.role);
        if let (Some(project_id), Some(root_id)) = (ctx.project_id(), ctx.root_id()) {
            let project = read
                .projects()
                .project(project_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let root = read
                .work()
                .work(&root_id.0)?
                .ok_or(StorageError::ScopeDenied)?;
            if root.project_id != *project_id || project.lifecycle != "Active" {
                return Err(StorageError::ScopeDenied);
            }
            context.push_str(&format!(
                "\nProject: {}\nWork: {}",
                project.name, root.title
            ));
        }
        Ok(context)
    })?;
    let saved_role = std::mem::take(&mut material.instructions)
        .into_iter()
        .map(|block| block.text)
        .collect::<Vec<_>>()
        .join("\n");
    let owner_context = if saved_role.is_empty() {
        owner_context
    } else {
        format!("{owner_context}\nAssigned role instructions:\n{saved_role}")
    };
    let rubric = RUBRIC
        .iter()
        .map(|(tier, description)| format!("{tier}: {description}"))
        .collect::<Vec<_>>()
        .join("\n");
    material.instructions = vec![TextBlock {
        text: format!(
            "{CLASSIFIER_INSTRUCTIONS}\nReturn exactly one of Low, Mid, High, Max.\nRubric version {RUBRIC_VERSION}\n{rubric}"
        ),
        cache: None,
    }];
    material.messages.insert(
        0,
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: owner_context,
                cache: None,
            }],
        },
    );
    material.parameters = GenerationParameters {
        max_output_tokens: Some(OUTPUT_TOKENS as u32),
        ..GenerationParameters::default()
    };
    let source = capture_source(engine, ctx, origin, material, cancel)?;
    let record = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        read.router()
            .auto_route(session)?
            .ok_or(StorageError::ScopeDenied)
    })?;
    if record.decision.is_some() {
        return check_cancel(cancel);
    }
    let (bytes, _) = engine
        .artifacts()
        .read_exact(
            engine.store().as_ref(),
            ctx,
            &source.material,
            8 * 1024 * 1024,
        )
        .map_err(|_| StorageError::ScopeDenied)?;
    let material: ProviderContext = serde_json::from_slice(&bytes).map_err(invalid)?;
    let (text, media) = material_for_decision(&material)?;

    if let Some(systemone) = &engine.auto_decisions
        && let Some(classification) =
            classify_systemone(engine, systemone, ctx, &source, &text, &media, cancel).await?
    {
        return commit_assignment(engine, ctx, &source, classification, cancel);
    }
    let mut required = vec![format!(
        "context:{}",
        source
            .material
            .byte_length
            .get()
            .saturating_add(OUTPUT_TOKENS)
    )];
    if !media.is_empty() {
        required.push("vision".into());
    }
    let router = engine
        .inference_router
        .clone()
        .ok_or(StorageError::ScopeDenied)?;
    let candidates = loop {
        check_cancel(cancel)?;
        let (ctx, session, required, router) = (
            ctx.clone(),
            session.clone(),
            required.clone(),
            router.clone(),
        );
        let plan = crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
            auto_routing::current_authority(tx, &ctx)?;
            router.auto_low_candidates(tx, &session, &required, false, &[])
        })
        .await?;
        match plan {
            Ok(plan) => break plan,
            Err(blocked) => match blocked.unavailable {
                devworks_core::services::router::select::Unavailable::Cooling(until) => {
                    wait_until(engine, until, cancel).await?;
                }
                _ => {
                    return Err(failure(
                        format!("No eligible Low classifier: {}.", blocked.codes.join(", ")),
                        RetryAdvice::AfterBlockerClears,
                    ));
                }
            },
        }
    };
    let mut next = Some(candidates);
    let mut cooling = std::collections::HashMap::<Id, i64>::new();
    loop {
        let candidates = match next.take() {
            Some(candidates) => candidates,
            None => {
                let now = engine.clock.now_ms().get();
                let elapsed = cooling
                    .iter()
                    .filter(|(_, until)| **until <= now)
                    .map(|(model, _)| model.clone())
                    .collect::<Vec<_>>();
                let (ctx, session, required, router) = (
                    ctx.clone(),
                    session.clone(),
                    required.clone(),
                    router.clone(),
                );
                let plan =
                    crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
                        auto_routing::current_authority(tx, &ctx)?;
                        router.auto_low_candidates(tx, &session, &required, true, &elapsed)
                    })
                    .await?;
                match plan {
                    Ok(plan) if !plan.is_empty() => plan,
                    other => {
                        let from_router =
                            other.err().and_then(|blocked| match blocked.unavailable {
                                devworks_core::services::router::select::Unavailable::Cooling(
                                    until,
                                ) => Some(until),
                                _ => None,
                            });
                        let earliest = from_router
                            .into_iter()
                            .chain(cooling.values().copied().filter(|until| *until > now))
                            .min();
                        if let Some(until) = earliest {
                            wait_until(engine, until, cancel).await?;
                            continue;
                        }
                        break;
                    }
                }
            }
        };
        for candidate in candidates {
            check_cancel(cancel)?;
            if candidate.ordinal != 0 {
                cooling.remove(&candidate.model_id);
            }
            match classify_low(
                engine,
                ctx,
                &source,
                &candidate,
                &required,
                !media.is_empty(),
                cancel,
            )
            .await?
            {
                LowResult::Assigned(classification) => {
                    return commit_assignment(engine, ctx, &source, classification, cancel);
                }
                LowResult::Cooling(until) if candidate.ordinal == 0 => {
                    cooling.insert(candidate.model_id, until);
                }
                _ => {}
            }
        }
    }
    if read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        Ok(read
            .router()
            .auto_route(session)?
            .is_some_and(|record| record.decision.is_some()))
    })? {
        return check_cancel(cancel);
    }
    Err(failure(
        "No eligible classifier returned a valid tier.".into(),
        RetryAdvice::AfterBlockerClears,
    ))
}

fn check_cancel(cancel: &Cancellation) -> Result<(), TurnError> {
    if cancel.is_cancelled() {
        return Err(StorageError::Cancelled.into());
    }
    Ok(())
}

fn invalid(error: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(format!("automatic route: {error}"))
}

fn failure(detail: String, retry: RetryAdvice) -> TurnError {
    TurnError::Command { detail, retry }
}

fn capture_source(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    origin: String,
    material: ProviderContext,
    cancel: &Cancellation,
) -> Result<Source, StorageError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    if let Some(current) =
        read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            read.router().auto_route(session)
        })?
    {
        return serde_json::from_value(current.source).map_err(invalid);
    }
    material
        .validate()
        .map_err(|_| invalid("invalid initial assignment"))?;
    let bytes = serde_json::to_vec(&material).map_err(invalid)?;
    let provenance =
        json!({"kind":"AutoTierInput","sessionId":session,"rubricVersion":RUBRIC_VERSION});
    let staged = engine
        .artifacts()
        .stage_host_bytes(engine.store().as_ref(), &bytes, provenance.clone())
        .map_err(|_| StorageError::ScopeDenied)?
        .map_err(|_| StorageError::ScopeDenied)?;
    let (artifacts, pending, ctx, session, cancel) = (
        engine.artifacts().clone(),
        staged.clone(),
        ctx.clone(),
        session.clone(),
        cancel.clone(),
    );
    let committed = run_write(
        engine.store().as_ref(),
        WriteOptions::observed(),
        move |tx| {
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            if let Some(current) = tx.router().auto_route(&session)? {
                return Ok((
                    serde_json::from_value(current.source).map_err(invalid)?,
                    false,
                ));
            }
            let fences = auto_routing::current_authority(tx, &ctx)?;
            if tx.router().session_pin(&session)?.is_some() {
                return Err(StorageError::ScopeDenied);
            }
            let policy_digest = fences
                .into_iter()
                .find_map(|fence| match fence {
                    devworks_core::ports::AuthorityFence::Policy { digest, .. } => Some(digest),
                    _ => None,
                })
                .ok_or(StorageError::ScopeDenied)?;
            let version = artifacts.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: "DevWorks Auto initial assignment",
                    media_type: "application/vnd.devworks.provider-context+json",
                    provenance,
                    scope: ctx
                        .project_id()
                        .cloned()
                        .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                    owner: ctx.principal().clone(),
                },
            )?;
            let source = Source {
                material: version_ref(&version),
                origin,
                rubric_version: RUBRIC_VERSION,
                authority_digest: policy_digest,
                criteria: RUBRIC
                    .iter()
                    .map(|(name, description)| (name.to_string(), description.to_string()))
                    .collect(),
                classifier_instructions: CLASSIFIER_INSTRUCTIONS.into(),
            };
            tx.router()
                .begin_auto_route(&session, &serde_json::to_value(&source).map_err(invalid)?)?;
            Ok((source, true))
        },
    );
    engine.artifacts().settle_host_bytes(
        engine.store().as_ref(),
        &staged,
        matches!(committed, Ok((_, true))),
    );
    committed.map(|(source, _)| source)
}

fn material_for_decision(
    material: &ProviderContext,
) -> Result<(Vec<String>, Vec<MediaRef>), StorageError> {
    let mut text = Vec::new();
    let mut media = Vec::new();
    for block in material
        .messages
        .iter()
        .flat_map(|message| &message.content)
    {
        match block {
            ContentBlock::Text { text: value, .. } => text.push(value.clone()),
            ContentBlock::Image { artifact, .. } => media.push(MediaRef::Artifact {
                artifact: artifact.clone(),
            }),
            _ => return Err(invalid("unsupported initial assignment content")),
        }
    }
    Ok((text, media))
}

async fn classify_systemone(
    engine: &TurnEngine,
    service: &std::sync::Arc<devworks_core::services::router::decisions::DecisionService>,
    ctx: &InvocationContext,
    source: &Source,
    text: &[String],
    media: &[MediaRef],
    cancel: &Cancellation,
) -> Result<Option<Assignment>, TurnError> {
    let request = DecideInput {
        model_id: None,
        expected_model_revision: None,
        state: DynamicJson(
            json!({"rubricVersion": source.rubric_version, "initialAssignment": text}),
        ),
        questions: BoundedList(vec![NamedQuestion {
            name: BoundedText("tier".into()),
            question: DecisionQuestion::Choice {
                instructions: BoundedText(source.classifier_instructions.clone()),
                criteria: BoundedList(
                    source
                        .criteria
                        .iter()
                        .map(|(name, description)| DecisionCriterion {
                            name: BoundedText(name.clone()),
                            description: Some(BoundedText(description.clone())),
                        })
                        .collect(),
                ),
                instruction_parts: None,
            },
        }]),
        images: BoundedList(media.to_vec()),
        videos: BoundedList(Vec::new()),
        timeout_ms: PositiveCounter("30000".into()),
        use_pool: Some(true),
    };
    let operation = {
        let (service, ctx, cancel) = (service.clone(), ctx.clone(), cancel.clone());
        crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            service.enqueue_auto(tx, &ctx, &request)
        })
        .await?
    };
    let Some(operation) = operation else {
        return Ok(None);
    };
    let mut changes = engine.commit_changes();
    let wake = std::sync::Arc::new(Wake::default());
    cancel.wake_on_cancel(&wake);
    loop {
        let before = wake.generation();
        check_cancel(cancel)?;
        match crate::providers::systemone::completion(engine.store().as_ref(), ctx, &operation)
            .map_err(|fault| {
                failure(
                    format!("SystemOne classification: {:?}", fault.code),
                    fault.retry,
                )
            })? {
            Some(OperationCompletion::Succeeded {
                result: OperationResult::RouterDecide { value },
                ..
            }) => {
                let tier = tier_at(&value.output.value.0);
                return Ok(tier.map(|tier| {
                    assignment(
                        source,
                        tier,
                        ClassifierKind::SystemOne,
                        value.model_id,
                        value.request_id,
                    )
                }));
            }
            Some(OperationCompletion::Failed { fault, .. }) => {
                return match fault.code {
                    DomainErrorCode::BudgetHold | DomainErrorCode::PermissionDenied => {
                        Err(failure(
                            format!("SystemOne classification: {:?}", fault.code),
                            fault.retry,
                        ))
                    }
                    DomainErrorCode::Cancelled => Err(StorageError::Cancelled.into()),
                    _ => Ok(None),
                };
            }
            Some(_) => {
                return Err(failure(
                    "The SystemOne classification outcome is unknown.".into(),
                    RetryAdvice::AfterBlockerClears,
                ));
            }
            None => {}
        }
        tokio::select! {
            _ = wake.changed_after(before) => {},
            _ = changes.changed() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

fn tier_at(value: &Value) -> Option<TierName> {
    value
        .pointer("/answers/tier/choice")
        .and_then(Value::as_str)
        .and_then(auto_routing::parse_tier)
}

fn assignment(
    source: &Source,
    tier: TierName,
    classifier: ClassifierKind,
    model_id: Id,
    request_id: Id,
) -> Assignment {
    Assignment {
        tier,
        rubric_version: source.rubric_version,
        input_digest: source.material.sha256.as_str().to_owned(),
        classifier,
        model_id,
        request_id,
    }
}

fn commit_assignment(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &Source,
    decision: Assignment,
    cancel: &Cancellation,
) -> Result<(), TurnError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    let encoded = serde_json::to_value(decision).map_err(invalid)?;
    let (ctx, session, material, cancelling) = (
        ctx.clone(),
        session.clone(),
        source.material.clone(),
        cancel.clone(),
    );
    run_write(
        engine.store().as_ref(),
        WriteOptions::observed(),
        move |tx| {
            if cancelling.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            auto_routing::current_authority(tx, &ctx)?;
            let current = tx
                .router()
                .auto_route(&session)?
                .ok_or(StorageError::ScopeDenied)?;
            let saved: Source = serde_json::from_value(current.source).map_err(invalid)?;
            if material.sha256 != saved.material.sha256 {
                return Err(StorageError::ScopeDenied);
            }
            tx.router().finish_auto_route(&session, &encoded)?;
            Ok(())
        },
    )?;
    // Cancellation after the decision commits must still stop the generation of this request.
    check_cancel(cancel)
}

enum LowResult {
    Assigned(Assignment),
    Cooling(i64),
    Unusable,
}

async fn classify_low(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &Source,
    candidate: &LowCandidate,
    required: &[String],
    has_images: bool,
    cancel: &Cancellation,
) -> Result<LowResult, TurnError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    let providers = engine.providers().ok_or(StorageError::ScopeDenied)?;
    let identity = sha256_hex(
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            session.as_str(),
            source.material.sha256.as_str(),
            candidate.model_id.as_str(),
            candidate.connection_id.as_str(),
            candidate.model_revision,
            candidate.account_revision,
            candidate.ordinal,
        )
        .as_bytes(),
    );
    let (request_id, recorded) = recover_low_attempt(engine, &identity)?;
    let result = if let Some(result) = recorded {
        result
    } else {
        let Some(invocation) = low_invocation(
            engine,
            ctx,
            source,
            candidate,
            required,
            has_images,
            &request_id,
        )?
        else {
            return Ok(LowResult::Unusable);
        };
        let produced = match providers
            .produce_cancellable(invocation, Some(cancel.clone()))
            .await
        {
            Ok(produced) => produced,
            Err(StorageError::Cancelled) => return Err(StorageError::Cancelled.into()),
            Err(error) => match recover_low_attempt(engine, &identity)? {
                (_, Some(result)) => {
                    return low_classification_from_result(
                        engine, ctx, source, candidate, request_id, result,
                    );
                }
                _ if !matches!(error, StorageError::CommitUnknown | StorageError::Busy) => {
                    // A source that failed before the durable transport boundary is safe to skip.
                    let ctx = ctx.clone();
                    crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
                        auto_routing::current_authority(tx, &ctx).map(|_| ())
                    })
                    .await?;
                    return Ok(LowResult::Unusable);
                }
                _ => return Err(error.into()),
            },
        };
        match produced.accounting {
            DriveOutcome::Refused(Admission::Blocked(block)) => {
                return Err(failure(
                    format!("Low classifier blocked by feature budget: {block:?}"),
                    RetryAdvice::AfterBlockerClears,
                ));
            }
            DriveOutcome::Refused(Admission::Fenced(fence)) => {
                return Err(failure(
                    format!("Low classifier authority changed: {fence:?}"),
                    RetryAdvice::AfterBlockerClears,
                ));
            }
            DriveOutcome::Refused(_) => return Ok(LowResult::Unusable),
            DriveOutcome::CancelledBeforeDispatch => return Err(StorageError::Cancelled.into()),
            _ => produced.result,
        }
    };
    low_classification_from_result(engine, ctx, source, candidate, request_id, result)
}

/// A logical receipt may precede admission. Only an attempt that could have dispatched
/// can prevent retrying this exact source, and only a settled provider result is replayed.
fn recover_low_attempt(
    engine: &TurnEngine,
    identity: &str,
) -> Result<(Id, Option<ProviderResult>), TurnError> {
    let providers = engine.providers().ok_or(StorageError::ScopeDenied)?;
    for ordinal in 0..16 {
        let request = Id::new(format!("auto-low-{identity}-{ordinal}")).map_err(invalid)?;
        let state = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            let Some(route) = read.router().route_request(&request)? else {
                return Ok(None);
            };
            let operation = read
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            for last in operation.attempts.iter().rev() {
                if let Some(attempt) = read.usage().inference_attempt(&last.id.0)? {
                    let balance = read
                        .usage()
                        .attempt_balance(&attempt.attempt_id)?
                        .map(|balance| {
                            devworks_core::services::usage::accounting::summary(&balance)
                        })
                        .transpose()?;
                    return Ok(Some((
                        attempt.state,
                        balance.and_then(|summary| summary.outcome),
                    )));
                }
            }
            Ok(None)
        })?;
        match state {
            None => return Ok((request, None)),
            Some((AttemptState::CancelledBeforeDispatch, _)) => continue,
            Some((state, outcome)) => {
                let mut result = providers.recorded_result(&request, &request)?;
                if result.outcome.is_none()
                    && state == AttemptState::Settled
                    && outcome.as_deref() == Some("Failed")
                {
                    // Encoding or another known pre-transport failure can settle Usage
                    // without a materialized provider sink.
                    result.outcome = Some(AttemptEnd::Failed);
                }
                return Ok((request, Some(result)));
            }
        }
    }
    Err(failure(
        "The Low classifier reached its safe admission retry limit.".into(),
        RetryAdvice::Never,
    ))
}

fn low_classification_from_result(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &Source,
    candidate: &LowCandidate,
    request_id: Id,
    result: ProviderResult,
) -> Result<LowResult, TurnError> {
    if result.outcome == Some(AttemptEnd::UnknownTail) || result.outcome.is_none() {
        return Err(failure(
            "The Low classifier outcome is unknown.".into(),
            RetryAdvice::AfterBlockerClears,
        ));
    }
    if result.outcome != Some(AttemptEnd::Completed) {
        return Ok(result
            .retry_at_ms
            .map_or(LowResult::Unusable, LowResult::Cooling));
    }
    let Some(output) = result.result_artifact else {
        return Ok(LowResult::Unusable);
    };
    let (bytes, _) = engine
        .artifacts()
        .read_exact(engine.store().as_ref(), ctx, &output, 1024)
        .map_err(|_| StorageError::ScopeDenied)?;
    let tier = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| auto_routing::parse_tier(text.trim()));
    Ok(tier.map_or(LowResult::Unusable, |tier| {
        LowResult::Assigned(assignment(
            source,
            tier,
            ClassifierKind::Low,
            candidate.model_id.clone(),
            request_id,
        ))
    }))
}

fn low_invocation(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &Source,
    candidate: &LowCandidate,
    required: &[String],
    has_images: bool,
    request_id: &Id,
) -> Result<Option<NativeInvocation>, StorageError> {
    let session = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    let info = engine.store().info();
    let fences = {
        let ctx = ctx.clone();
        run_write(
            engine.store().as_ref(),
            WriteOptions::observed(),
            move |tx| auto_routing::current_authority(tx, &ctx),
        )?
    };
    read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        let Some(model) = read.router().route_model(&candidate.model_id)? else {
            return Ok(None);
        };
        let Some(connection) = read.router().connection(&candidate.connection_id)? else {
            return Ok(None);
        };
        if model.revision != candidate.model_revision
            || connection.account_revision != candidate.account_revision
            || model.upstream_model_id != candidate.upstream
            || connection.profile != candidate.endpoint
        {
            return Ok(None);
        }
        let owner =
            auxiliary::ownership(read, &info, ctx).map_err(|_| StorageError::ScopeDenied)?;
        let root = ctx.root_id();
        let control = root
            .map(|id| read.runs().root_control(id))
            .transpose()?
            .flatten();
        let attribution = match (root, ctx.project_id()) {
            (Some(root), Some(project)) => {
                let work = read
                    .work()
                    .work(&root.0)?
                    .ok_or(StorageError::ScopeDenied)?;
                if work.project_id != *project {
                    return Err(StorageError::ScopeDenied);
                }
                if work.is_feature() {
                    Attribution::Feature {
                        project_id: project.clone(),
                        root_id: root.clone(),
                        assignment_id: ctx.assignment_id().cloned(),
                        job_run_id: owner.job_run_id.clone(),
                    }
                } else {
                    Attribution::Issue {
                        project_id: project.clone(),
                        root_id: root.clone(),
                        assignment_id: ctx.assignment_id().cloned(),
                        job_run_id: owner.job_run_id.clone(),
                    }
                }
            }
            (None, None) => Attribution::Workspace {
                session_id: session.clone(),
            },
            _ => return Err(StorageError::ScopeDenied),
        };
        let query = PriceQuery {
            catalog_model_id: model.id.clone(),
            provider_model_id: Some(model.upstream_model_id),
            connection_id: connection.id.clone(),
            endpoint_profile: connection.profile,
            service_tier: None,
            input_tokens: None,
            cache_write_ttl_ms: None,
            processing_region: None,
            required_modalities: if has_images {
                vec!["Text".into(), "Image".into()]
            } else {
                vec!["Text".into()]
            },
        };
        let (generation, instruction, stop) = control.map_or((1, 1, 1), |c| {
            (
                c.generation,
                c.instruction_revision.max(1),
                c.stop_epoch.saturating_add(1),
            )
        });
        let revision = |n| Revision::new(n).map_err(invalid);
        let logical = RequestAdmission {
            request_id: request_id.clone(),
            purpose: Purpose::TierClassification,
            session_id: Some(session.clone()),
            project_id: ctx.project_id().cloned(),
            root_id: root.cloned(),
            model_id: model.id.clone(),
            connection_id: connection.id,
            pin_id: None,
            args_digest: String::new(),
        };
        let attempt = AttemptRequest {
            admission_key: request_id.clone(),
            request_id: request_id.clone(),
            actor_id: ctx.principal().clone(),
            work_id: owner.work_id,
            job_run_id: owner.job_run_id,
            role: owner.role.clone(),
            quote: CatalogPriceResolver.quote(read, &query)?,
            price_generation: read.usage().price_revision()?,
            override_revision: read.usage().price_override_revision(&model.id)?,
            query,
            pricing: PricingContext {
                input_semantics: InputSemantics::Unknown,
                cache_read_possible: false,
                cache_write_possible: false,
                cache_write_ttl_ms: None,
            },
            selection: AttemptSelection::TierClassifier {
                model_revision: candidate.model_revision,
                account_revision: candidate.account_revision,
                required_capabilities: required.to_vec(),
            },
            input_tokens: source.material.byte_length.get(),
            output_headroom: OUTPUT_TOKENS,
            unit_headroom: Vec::new(),
            expected_context_epoch: owner.expected_context_epoch,
            dispatch: owner.dispatch,
            cache_diagnostics: Vec::new(),
        };
        Ok(Some(NativeInvocation {
            source_fences: fences,
            instruction_targets: Some(Vec::new()),
            context: ctx.clone(),
            logical,
            category: ReportCategory::for_agent_role(&owner.role),
            attempt,
            request: InferenceRequest {
                options: InferenceOptions {
                    selection: ModelSelection::Model { model_id: model.id },
                    effort: None,
                    output_schema: None,
                    deadline_ms: 30000,
                },
                authority_revision: ctx.policy_revision().clone(),
                control_generation: revision(generation)?,
                instruction_revision: revision(instruction)?,
                cancellation_revision: revision(stop)?,
                protocol: Protocol::Draft1,
                request_id: request_id.clone(),
                purpose: InferencePurpose::Semantic,
                attribution,
                input: InferenceInput::Generation {
                    session_id: session.clone(),
                    context: source.material.clone(),
                    context_epoch: Counter::new(owner.expected_context_epoch.unwrap_or(0))
                        .map_err(invalid)?,
                    tools: Vec::new(),
                    response_headroom_tokens: revision(OUTPUT_TOKENS)?,
                    requires_images: has_images,
                    requires_parallel_tools: false,
                },
            },
            nested: None,
        }))
    })
}

pub(crate) fn cooldown(
    engine: &TurnEngine,
    session: &Id,
    intent: &RouteIntent,
    effort: &EffortName,
    required: &[String],
) -> Result<Option<i64>, StorageError> {
    let router = engine
        .inference_router
        .as_ref()
        .ok_or(StorageError::ScopeDenied)?;
    read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        let pin = read.router().session_pin(session)?;
        let capabilities: Vec<_> = required.iter().map(String::as_str).collect();
        router
            .availability_of(
                read,
                intent,
                effort,
                &capabilities,
                (Some(session), pin.as_ref()),
                false,
            )
            .map(|route| route.retry_at_ms.map(|time| time.get()))
            .map_err(|failure| failure.into_storage())
    })
}

pub(crate) async fn wait_until(
    engine: &TurnEngine,
    until_ms: i64,
    cancel: &Cancellation,
) -> Result<(), TurnError> {
    let now = engine.clock.now_ms().get();
    let delay = Duration::from_millis(until_ms.saturating_sub(now).max(1) as u64);
    let wake = std::sync::Arc::new(Wake::default());
    cancel.wake_on_cancel(&wake);
    let before = wake.generation();
    check_cancel(cancel)?;
    tokio::select! {
        _ = wake.changed_after(before) => check_cancel(cancel),
        _ = tokio::time::sleep(delay) => check_cancel(cancel),
    }
}
