//! Native desktop use case. The resource lease is the durable control fence; an exact reviewed
//! Human decision is the consent. Capture and input remain separate OS capabilities.
use crate::admission::DecodedRequest;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{Fingerprint, sha256_hex};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, desktop as ops, resources as resource_ops};
use crate::ports::desktop::{DesktopAvailability, DesktopCapabilities, DesktopPlatform};
use crate::ports::storage::{
    ApplicationStore, ApprovalSource, ArtifactScope, ConfigScope, LeasePhase, NewEvent,
    NewOperation, NewQueueEntry, OperationRecord, ReadOptions, ReadSnapshot, ResourceRead,
    ResourceScopeKind, StorageError, StoredResource, WriteOptions, WriteTransaction, canonical_key,
    read_snapshot, run_write,
};
use crate::ports::{
    AuthorityPort, AuthorityRequest, Clock, CommandHandler, Decided, HandlerFailure, IdKind,
    IdSource, Preparation, Prepared, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{DesktopCapture, DesktopRecord, ItemState};
use crate::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use crate::protocol::support::{BoundedText, Check, Limit};
use crate::protocol::{
    ActorId, AssignmentId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, OperationId,
    OperationState, ProjectId, Protocol, ResourceId, RetryAdvice, RootId,
};
use crate::registry::Mode;
use crate::services::approvals::OwnerPreview;
use crate::services::artifacts::ArtifactService;
use crate::services::artifacts::evidence::{Evidence, EvidenceFailure};
use crate::services::sessions::{Failure, Refusal, counter, decided, query_fault, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

pub const OPERATIONS: [&str; 5] = [
    "desktop.inspect",
    "desktop.capture",
    "desktop.request_control",
    "desktop.input",
    "desktop.release_control",
];
const CONSENT_PURPOSE: &str = "desktop-consent:";
pub const INPUT_CONSUMER: &str = "desktop_input_effects";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct InputJob {
    actor: ActorId,
    kind: String,
    session: Option<Id>,
    project: Option<ProjectId>,
    root: Option<RootId>,
    assignment: Option<AssignmentId>,
    restore_epoch: i64,
    policy_revision: i64,
    authority_sources: Vec<Id>,
    fingerprint: String,
    input: ops::InputInput,
}

impl InputJob {
    fn new(ctx: &InvocationContext, command: &AdmittedCommand, input: &ops::InputInput) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: format!("{:?}", ctx.principal_kind()),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            authority_sources: ctx.authority_sources().to_vec(),
            fingerprint: command.fingerprint().as_str().into(),
            input: input.clone(),
        }
    }

    fn context(&self, store: &dyn ApplicationStore, operation: &OperationId) -> InvocationContext {
        let kind = match self.kind.as_str() {
            "Human" => PrincipalKind::Human,
            "Agent" => PrincipalKind::Agent,
            "Package" => PrincipalKind::Package,
            "McpClient" => PrincipalKind::McpClient,
            "Schedule" => PrincipalKind::Schedule,
            _ => PrincipalKind::Host,
        };
        let mut ctx = InvocationContext::new(
            self.actor.clone(),
            kind,
            operation.0.clone(),
            Id::new(INPUT_CONSUMER).expect("static consumer"),
            store.info().store_id,
            counter(self.restore_epoch),
            rev(self.policy_revision),
        )
        .with_lineage(
            self.project.clone(),
            self.root.clone(),
            self.assignment.clone(),
        );
        if let Some(session) = &self.session {
            ctx = ctx.with_session(session.clone());
        }
        for source in &self.authority_sources {
            ctx = ctx.with_authority_source(source.clone());
        }
        ctx
    }
}

pub enum InputEffectOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
}

pub struct DesktopService {
    platform: Arc<dyn DesktopPlatform>,
    resources: Arc<crate::services::resources::ResourceService>,
    artifacts: Arc<ArtifactService>,
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    authority: Arc<dyn AuthorityPort>,
}

impl DesktopService {
    pub fn new(
        platform: Arc<dyn DesktopPlatform>,
        resources: Arc<crate::services::resources::ResourceService>,
        artifacts: Arc<ArtifactService>,
        store: Arc<dyn ApplicationStore>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdSource>,
        authority: Arc<dyn AuthorityPort>,
    ) -> Self {
        Self {
            platform,
            resources,
            artifacts,
            store,
            clock,
            ids,
            authority,
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(crate::ports::IdKind::Correlation))
    }

    /// Scoped typed completion for an accepted input effect. Success attests OS injection only;
    /// a UI test must separately assert the application outcome.
    pub fn completion(
        &self,
        ctx: &InvocationContext,
        operation_id: &OperationId,
    ) -> Result<Option<OperationCompletion>, DomainFault> {
        let record = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.operations().operation(operation_id)
        })
        .map_err(|error| query_fault(error.into(), "operation", self.correlation()))?;
        let not_found = || {
            self.fault(
                DomainErrorCode::NotFound,
                "operation",
                Some(operation_id.0.clone()),
            )
        };
        let operation = record.ok_or_else(not_found)?;
        let job = if operation.capability == "desktop.input" {
            serde_json::from_value::<InputJob>(operation.payload.clone()).ok()
        } else {
            None
        }
        .ok_or_else(not_found)?;
        if job.project.as_ref() != ctx.project_id()
            || (job.actor != *ctx.principal()
                && !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
        {
            return Err(not_found());
        }
        let revision = rev(operation.revision);
        let effect = operation.latest_attempt().and_then(|attempt| {
            attempt.observations.iter().rev().find_map(|observation| {
                observation
                    .get("effect")
                    .or_else(|| observation.get("reconciliation"))
                    .cloned()
            })
        });
        Ok(match operation.state {
            OperationState::ObservedComplete => {
                let result = effect
                    .as_ref()
                    .and_then(|e| e.pointer("/evidence/result"))
                    .and_then(|value| {
                        serde_json::from_value::<OperationResult>(value.clone()).ok()
                    });
                Some(match result {
                    Some(result) => OperationCompletion::Succeeded {
                        protocol: Protocol::Draft1,
                        operation_id: operation.id,
                        revision,
                        result,
                    },
                    None => OperationCompletion::Failed {
                        protocol: Protocol::Draft1,
                        operation_id: operation.id,
                        revision,
                        fault: Refusal::new(DomainErrorCode::OutcomeUnknown, "desktop_input", None)
                            .retry(RetryAdvice::ReconcileOperation)
                            .fault(self.correlation()),
                    },
                })
            }
            OperationState::Failed | OperationState::CancelledBeforeDispatch => {
                Some(OperationCompletion::Failed {
                    protocol: Protocol::Draft1,
                    operation_id: operation.id,
                    revision,
                    fault: Refusal::new(
                        if operation.state == OperationState::Failed {
                            DomainErrorCode::ExternalFailure
                        } else {
                            DomainErrorCode::Cancelled
                        },
                        "desktop_input",
                        None,
                    )
                    .fault(self.correlation()),
                })
            }
            OperationState::OutcomeUnknown => Some(OperationCompletion::OutcomeUnknown {
                protocol: Protocol::Draft1,
                operation_id: operation.id,
                revision,
                correlation_id: self.correlation(),
            }),
            _ => None,
        })
    }

    fn record(
        &self,
        resources: &dyn ResourceRead,
    ) -> Result<(StoredResource, DesktopRecord), StorageError> {
        let key = canonical_key(
            ResourceScopeKind::Machine,
            None,
            crate::services::resources::DESKTOP,
        );
        let resource = resources
            .resource_by_key(&key)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "desktop",
                id: key.clone(),
            })?;
        let holder = resources.holders(&resource.id, 0, 1)?.into_iter().next();
        let consent_id = holder
            .as_ref()
            .and_then(|lease| lease.purpose.as_deref())
            .and_then(|purpose| purpose.strip_prefix(CONSENT_PURPOSE))
            .and_then(|id| Id::new(id).ok());
        let revision = if resource.fence_seq == 0 {
            1
        } else {
            resource
                .fence_seq
                .checked_mul(2)
                .and_then(|n| {
                    n.checked_add(
                        if holder
                            .as_ref()
                            .is_some_and(|lease| lease.phase == LeasePhase::Held)
                        {
                            2
                        } else {
                            3
                        },
                    )
                })
                .ok_or_else(|| StorageError::InvalidRecord("desktop revision overflow".into()))?
        };
        let capabilities = self.platform.capabilities();
        let state = if !matches!(capabilities.input, DesktopAvailability::Available)
            && !matches!(capabilities.capture, DesktopAvailability::Available)
        {
            ItemState::Unavailable
        } else if holder
            .as_ref()
            .is_some_and(|lease| lease.phase == LeasePhase::Held)
        {
            ItemState::Enabled
        } else if holder.is_some() {
            ItemState::Partial
        } else {
            ItemState::Disabled
        };
        let record = DesktopRecord {
            desktop_id: resource.id.clone(),
            revision: rev(revision),
            state,
            permission_state: BoundedText(permission_label(&capabilities)),
            lease_id: holder.as_ref().map(|lease| lease.id.clone()),
            consent_id,
        };
        record.check().map_err(StorageError::InvalidRecord)?;
        Ok((resource, record))
    }

    fn fault(&self, code: DomainErrorCode, kind: &'static str, id: Option<Id>) -> DomainFault {
        Refusal::new(code, kind, id).fault(self.correlation())
    }

    /// The generic saved-operation approval displays the exact takeover reason, desktop and
    /// lease bound. A Human OK resumes this same command; Wait leaves it pending.
    pub fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let OperationInput::Desktop(ops::Input::RequestControl(input)) = &request.input else {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "desktop_approval",
                None,
            )
            .into());
        };
        let (_, record) = env.read(|snapshot| self.record(snapshot.resources()))?;
        self.preview(ctx, input, &record)
    }

    pub fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        let OperationInput::Desktop(ops::Input::RequestControl(input)) = &request.input else {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "desktop_approval",
                None,
            )
            .into());
        };
        let (_, record) = self.record(tx.resources())?;
        let current = self.preview(ctx, input, &record)?;
        if current.scope != preview.scope
            || current.target_id != preview.target_id
            || current.target_revision != preview.target_revision
            || current.binding != preview.binding
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "desktop_approval",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        Ok(())
    }

    fn preview(
        &self,
        ctx: &InvocationContext,
        input: &ops::RequestControlInput,
        record: &DesktopRecord,
    ) -> Result<OwnerPreview, Failure> {
        if input.desktop_id != record.desktop_id {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        if input.expected_revision != record.revision {
            return Err(Refusal::new(
                DomainErrorCode::RevisionConflict,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        if !matches!(
            self.platform.capabilities().input,
            DesktopAvailability::Available
        ) {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "desktop_input",
                None,
            )
            .into());
        }
        let scope = ctx
            .project_id()
            .cloned()
            .map_or(ConfigScope::Workspace, ConfigScope::Project);
        let binding = json!({"desktopId":input.desktop_id,"revision":input.expected_revision,"reason":input.reason,"leaseMs":input.lease_ms,"permissionState":record.permission_state});
        Ok(OwnerPreview {
            scope,
            target_id: input.desktop_id.clone(),
            target_revision: input.expected_revision.get(),
            summary: format!("Allow desktop control: {}", input.reason.0),
            reviewed: binding.clone(),
            binding,
        })
    }

    fn request_control(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::RequestControlInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        let (resource, before) = self.record(tx.resources())?;
        if input.desktop_id != resource.id {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        if input.expected_revision != before.revision {
            return Err(Refusal::new(
                DomainErrorCode::RevisionConflict,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        if !matches!(
            self.platform.capabilities().input,
            DesktopAvailability::Available
        ) {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "desktop_input",
                None,
            )
            .into());
        }
        // An unknown idle state always asks the owner for OK. The reviewed context is minted
        // only from an approved SavedOperation and verified against this exact request.
        let Some(consent) = ctx
            .reviewed_operation()
            .map(|reviewed| reviewed.approval_id().clone())
        else {
            return Err(
                Refusal::new(DomainErrorCode::ApprovalRequired, "desktop_takeover", None).into(),
            );
        };
        let binding =
            crate::services::approvals::owner_preview::approved_binding(tx, ctx, command)?;
        if binding.is_none() {
            return Err(
                Refusal::new(DomainErrorCode::ApprovalRequired, "desktop_takeover", None).into(),
            );
        }
        let purpose = format!("{CONSENT_PURPOSE}{}", consent.as_str());
        let acquire = resource_ops::AcquireInput {
            resource_id: ResourceId(
                Id::new(crate::services::resources::DESKTOP).expect("static desktop ID"),
            ),
            expected_revision: rev(resource.revision),
            units: Limit::new(1).expect("one unit"),
            lease_ms: input.lease_ms.clone(),
            purpose: Some(BoundedText(purpose.clone())),
            priority: None,
            capacity: None,
            labels: None,
        };
        let allocated = self.resources.acquire(tx, ctx, &acquire)?;
        let (applied, lease) = match allocated {
            DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire { value },
            } => (true, value),
            DomainDecision::NoChange {
                result: OperationResult::ResourcesAcquire { value },
            } => (false, value),
            _ => return Err(StorageError::InvalidRecord("desktop lease decision".into()).into()),
        };
        let stored = tx
            .resources()
            .lease(&lease.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "desktop_lease",
                id: lease.id.as_str().to_owned(),
            })?;
        if stored.purpose.as_deref() != Some(purpose.as_str()) {
            return Err(
                Refusal::new(DomainErrorCode::Conflict, "desktop_consent", Some(lease.id)).into(),
            );
        }
        let (_, mut record) = self.record(tx.resources())?;
        if stored.phase == LeasePhase::Waiting {
            record.state = ItemState::Partial;
            record.lease_id = Some(stored.id);
            record.consent_id = Some(consent);
        }
        let result = OperationResult::DesktopRequestControl { value: record };
        Ok(if applied {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        })
    }

    fn release_control(
        &self,
        ctx: &InvocationContext,
        input: &ops::ReleaseControlInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        let (resource, _) = self.record(tx.resources())?;
        if input.desktop_id != resource.id {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        let lease = tx.resources().lease(&input.lease_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "desktop_lease",
                Some(input.lease_id.clone()),
            )
        })?;
        if lease.resource_id != resource.id
            || !lease
                .purpose
                .as_deref()
                .is_some_and(|purpose| purpose.starts_with(CONSENT_PURPOSE))
        {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "desktop_lease",
                Some(input.lease_id.clone()),
            )
            .into());
        }
        let released = self.resources.release(
            tx,
            ctx,
            &resource_ops::ReleaseInput {
                lease_id: input.lease_id.clone(),
                expected_fence: input.expected_fence.clone(),
            },
        )?;
        let (_, record) = self.record(tx.resources())?;
        let result = OperationResult::DesktopReleaseControl { value: record };
        Ok(match released {
            DomainDecision::Applied { .. } => DomainDecision::Applied { result },
            DomainDecision::NoChange { .. } => DomainDecision::NoChange { result },
            _ => return Err(StorageError::InvalidRecord("desktop release decision".into()).into()),
        })
    }

    fn queue_input(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::InputInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<DomainDecision, Failure> {
        let (resource, record) = self.record(tx.resources())?;
        if input.desktop_id != resource.id || record.lease_id.as_ref() != Some(&input.lease_id) {
            return Err(Refusal::new(
                DomainErrorCode::NotFound,
                "desktop_lease",
                Some(input.lease_id.clone()),
            )
            .into());
        }
        if input.expected_desktop_revision != record.revision {
            return Err(Refusal::new(
                DomainErrorCode::RevisionConflict,
                "desktop",
                Some(input.desktop_id.clone()),
            )
            .into());
        }
        Self::validate_input(tx, ctx, input)?;
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let job = InputJob::new(ctx, command, input);
        let payload =
            serde_json::to_value(&job).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "desktop.input".into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(), actor: ctx.principal().clone(), project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(), operation_id: Some(operation.clone()),
            aggregate_kind: "desktop_input".into(), aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1, kind: "DesktopInputRequested".into(),
            payload: json!({"desktopId":resource.id.as_str(),"leaseId":input.lease_id.as_str(),"consentId":input.consent_id.as_str()}),
        })?;
        let now = tx.now_ms();
        let entry = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: INPUT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: Some(resource.id.as_str().into()),
            priority: 0,
            ready_at_ms: now,
            payload: json!({"operationId":operation.0.as_str()}),
        })?;
        Ok(DomainDecision::Accepted {
            operation_id: operation,
        })
    }

    fn validate_input(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::InputInput,
    ) -> Result<(), Failure> {
        let lease = tx.resources().lease(&input.lease_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "desktop_lease",
                Some(input.lease_id.clone()),
            )
        })?;
        if lease.owner != *ctx.principal()
            || lease.session_id.as_ref() != ctx.session_id()
            || lease.project_id.as_ref() != ctx.project_id()
            || lease.phase != LeasePhase::Held
            || lease.expires_at_ms <= tx.now_ms()
            || lease.fence != input.expected_fence.get()
            || lease.purpose.as_deref()
                != Some(format!("{CONSENT_PURPOSE}{}", input.consent_id.as_str()).as_str())
        {
            return Err(Refusal::new(
                DomainErrorCode::LeaseLost,
                "desktop_lease",
                Some(input.lease_id.clone()),
            )
            .into());
        }
        let approval = tx.decisions().approval(&input.consent_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent",
                Some(input.consent_id.clone()),
            )
        })?;
        let audit = tx
            .decisions()
            .human_decision(ApprovalSource::Generic, &input.consent_id)?;
        if approval.state != "Approved"
            || approval.operation_kind != "desktop.request_control"
            || approval.target_id != input.desktop_id
            || approval.author != *ctx.principal()
            || approval.scope
                != ctx
                    .project_id()
                    .cloned()
                    .map_or(ConfigScope::Workspace, ConfigScope::Project)
            || approval.restore_epoch != tx.store().restore_epoch
            || audit
                .as_ref()
                .is_none_or(|decision| decision.decision != "Approved")
            || !approval.decided_by.as_ref().is_some_and(|id| {
                tx.actors().actor_kind(id).ok().flatten() == Some(PrincipalKind::Human)
            })
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent",
                Some(input.consent_id.clone()),
            )
            .into());
        }
        Ok(())
    }

    /// An uncertain attempt is never replayed: a click or key may already have reached the OS.
    pub fn observe_input(&self, operation: &OperationRecord) -> InputEffectOutcome {
        if operation.capability != "desktop.input" {
            return InputEffectOutcome::Failed("unsupported desktop effect".into());
        }
        InputEffectOutcome::Unknown("desktop input outcome needs observation".into())
    }

    pub fn execute_input(&self, operation: &OperationRecord) -> InputEffectOutcome {
        if operation.capability != "desktop.input" || operation.schema_version != 1 {
            return InputEffectOutcome::Failed("unsupported desktop effect".into());
        }
        let Ok(job) = serde_json::from_value::<InputJob>(operation.payload.clone()) else {
            return InputEffectOutcome::Failed("invalid desktop intent".into());
        };
        let ctx = job.context(&*self.store, &operation.id);
        let Some(descriptor) = crate::registry::find("desktop.input") else {
            return InputEffectOutcome::Failed("desktop descriptor unavailable".into());
        };
        let Some(binding) = descriptor.binding(Mode::Command) else {
            return InputEffectOutcome::Failed("desktop command binding unavailable".into());
        };
        let Some(fingerprint) = Fingerprint::from_hex(&job.fingerprint) else {
            return InputEffectOutcome::Failed("invalid desktop fingerprint".into());
        };
        let input = OperationInput::Desktop(ops::Input::Input(job.input.clone()));
        if self
            .authority
            .authorize(
                &ctx,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &input,
                    fingerprint: &fingerprint,
                },
            )
            .is_err()
        {
            return InputEffectOutcome::Failed("current desktop authority denied dispatch".into());
        }
        let resources = self.resources.clone();
        let context = ctx.clone();
        let command = job.input.clone();
        let ticket = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let ticket = resources
                .authorize_dispatch(
                    tx,
                    &context,
                    "desktop",
                    &command.lease_id,
                    command.expected_fence.get(),
                )
                .map_err(|_| {
                    StorageError::InvalidRecord("desktop lease dispatch refused".into())
                })?;
            Self::validate_input(tx, &context, &command).map_err(|_| {
                StorageError::InvalidRecord("desktop consent dispatch refused".into())
            })?;
            Ok(ticket)
        });
        let Ok(ticket) = ticket else {
            return InputEffectOutcome::Failed("desktop lease or consent no longer valid".into());
        };
        let result = self.platform.inject(&job.input.action);
        drop(ticket);
        match result {
            Ok(()) => {
                let record = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
                    self.record(snapshot.resources()).map(|(_, record)| record)
                });
                match record {
                    Ok(record) => InputEffectOutcome::Completed(
                        json!({"result":OperationResult::DesktopInput { value: record },"actionApplied":true}),
                    ),
                    Err(_) => InputEffectOutcome::Unknown(
                        "desktop input applied but completion record unavailable".into(),
                    ),
                }
            }
            Err(crate::ports::desktop::DesktopFault::PermissionRequired) => {
                InputEffectOutcome::Failed("desktop input permission required".into())
            }
            Err(crate::ports::desktop::DesktopFault::Unsupported) => {
                InputEffectOutcome::Failed("desktop input unsupported".into())
            }
            Err(crate::ports::desktop::DesktopFault::NativeFailure) => {
                InputEffectOutcome::Unknown("desktop input native outcome unknown".into())
            }
        }
    }
}

fn permission_label(capabilities: &DesktopCapabilities) -> String {
    fn label(state: &DesktopAvailability) -> String {
        match state {
            DesktopAvailability::Available => "available".into(),
            DesktopAvailability::PermissionRequired(reason) => {
                format!("permission-required:{reason}")
            }
            DesktopAvailability::Unsupported(reason) => format!("unsupported:{reason}"),
        }
    }
    format!(
        "capture:{};input:{};accessibility-targets:unsupported:no-native-selector",
        label(&capabilities.capture),
        label(&capabilities.input)
    )
}

impl QueryHandler for DesktopService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let OperationInput::Desktop(ops::Input::Capture(input)) = query.input() else {
            return Ok(None);
        };
        let target = input.desktop_id.clone();
        let found = env
            .read(move |snapshot| {
                let key = canonical_key(
                    ResourceScopeKind::Machine,
                    None,
                    crate::services::resources::DESKTOP,
                );
                let resource = snapshot.resources().resource_by_key(&key)?;
                Ok(resource.is_some_and(|r| r.id == target))
            })
            .map_err(|e| query_fault(e.into(), "desktop", self.correlation()))?;
        if !found {
            return Err(self.fault(
                DomainErrorCode::NotFound,
                "desktop",
                Some(input.desktop_id.clone()),
            ));
        }
        let png = self.platform.capture_png().map_err(|fault| match fault {
            crate::ports::desktop::DesktopFault::PermissionRequired => {
                self.fault(DomainErrorCode::PermissionDenied, "screen_capture", None)
            }
            crate::ports::desktop::DesktopFault::Unsupported => self.fault(
                DomainErrorCode::UnsupportedCapability,
                "screen_capture",
                None,
            ),
            crate::ports::desktop::DesktopFault::NativeFailure => {
                self.fault(DomainErrorCode::ExternalFailure, "screen_capture", None)
            }
        })?;
        let evidence = Evidence {
            scope: ctx
                .project_id()
                .cloned()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
            producer: ctx.principal().clone(),
            principal_kind: format!("{:?}", ctx.principal_kind()),
            agent_session: ctx.session_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            title: "Desktop capture".into(),
            media_type: "image/png".into(),
            tags: vec!["desktop".into(), "capture".into()],
            source: serde_json::json!({"kind":"DesktopCapture","desktopId":input.desktop_id.as_str()}),
        };
        let (reference, ()) = self
            .artifacts
            .publish_evidence(&*self.store, evidence, &png, |_tx, _ref| Ok(()))
            .map_err(|e| match e {
                EvidenceFailure::Refused(refusal) => refusal.fault(self.correlation()),
                EvidenceFailure::Storage(error) => {
                    query_fault(error.into(), "artifact", self.correlation())
                }
            })?;
        Ok(Some(Prepared::new(reference)))
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let (_, record) = self
            .record(snapshot.resources())
            .map_err(|e| query_fault(e.into(), "desktop", self.correlation()))?;
        let result = match query.input() {
            OperationInput::Desktop(ops::Input::Inspect(_)) => {
                OperationResult::DesktopInspect { value: record }
            }
            OperationInput::Desktop(ops::Input::Capture(input)) => {
                if input.desktop_id != record.desktop_id {
                    return Err(self.fault(
                        DomainErrorCode::NotFound,
                        "desktop",
                        Some(input.desktop_id.clone()),
                    ));
                }
                if input.expected_revision != record.revision {
                    return Err(self.fault(
                        DomainErrorCode::RevisionConflict,
                        "desktop",
                        Some(input.desktop_id.clone()),
                    ));
                }
                let image = query
                    .prepared::<ArtifactRef>()
                    .ok_or_else(|| self.fault(DomainErrorCode::Conflict, "desktop_capture", None))?
                    .clone();
                OperationResult::DesktopCapture {
                    value: DesktopCapture {
                        desktop: record,
                        image,
                        captured_at_ms: self.clock.now_ms(),
                    },
                }
            }
            _ => return Err(self.fault(DomainErrorCode::UnsupportedCapability, "operation", None)),
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for DesktopService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let OperationInput::Desktop(ops::Input::Input(input)) = command.input() {
            return match self.queue_input(ctx, command, input, tx) {
                Ok(DomainDecision::Accepted { operation_id }) => Ok(Decided {
                    decision: DomainDecision::Accepted {
                        operation_id: operation_id.clone(),
                    },
                    operation_id: Some(operation_id),
                }),
                other => decided(tx, other),
            };
        }
        let decision = match command.input() {
            OperationInput::Desktop(ops::Input::RequestControl(input)) => {
                self.request_control(ctx, command, input, tx)
            }
            OperationInput::Desktop(ops::Input::ReleaseControl(input)) => {
                self.release_control(ctx, input, tx)
            }
            _ => Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "desktop_control",
                None,
            )
            .into()),
        };
        decided(tx, decision)
    }
}
