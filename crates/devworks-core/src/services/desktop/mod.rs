//! Native desktop use case. The resource lease is the durable control fence; an exact reviewed
//! Human decision is the consent. Capture and input remain separate OS capabilities.
use crate::admission::DecodedRequest;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::{Fingerprint, sha256_hex};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, desktop as ops, resources as resource_ops};
use crate::ports::desktop::{
    AccessibilitySnapshot, DesktopAvailability, DesktopCapabilities, DesktopPlatform,
};
use crate::ports::storage::{
    ApplicationStore, ApprovalSource, ArtifactScope, ConfigScope, LeasePhase, NewEvent,
    NewOperation, NewQueueEntry, OperationRecord, ReadOptions, ReadSnapshot, ResourceRead,
    ResourceScopeKind, StorageError, StoredLease, StoredResource, WriteOptions, WriteTransaction,
    canonical_key, read_snapshot, run_write,
};
use crate::ports::{
    AuthorityPort, AuthorityRequest, Clock, CommandHandler, Decided, HandlerFailure, IdKind,
    IdSource, Preparation, Prepared, QueryAnswer, QueryHandler,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    DesktopAccessibilityNode, DesktopAccessibilitySnapshot, DesktopCapture, DesktopRecord,
    ItemState,
};
use crate::protocol::results::{DomainDecision, OperationCompletion, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Check, Limit, RevisionDimension};
use crate::protocol::{
    ActorId, AssignmentId, CommandReply, CorrelationId, DomainErrorCode, DomainFault, EventId, Id,
    OperationId, OperationState, ProjectId, Protocol, ResourceId, RetryAdvice, RootId,
};
use crate::registry::Mode;
use crate::services::approvals::OwnerPreview;
use crate::services::artifacts::ArtifactService;
use crate::services::artifacts::evidence::{Evidence, EvidenceFailure};
use crate::services::git::context::SourceBinding;
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
const CONSENT_PURPOSE: &str = crate::services::resources::DESKTOP_CONSENT_PURPOSE;
pub const INPUT_CONSUMER: &str = "desktop_input_effects";

fn accessibility_record(
    snapshot: AccessibilitySnapshot,
) -> Result<DesktopAccessibilitySnapshot, String> {
    let revision = BoundedText(snapshot.revision());
    let record = DesktopAccessibilitySnapshot {
        revision,
        platform: BoundedText(snapshot.platform),
        nodes: BoundedList(
            snapshot
                .nodes
                .into_iter()
                .map(|node| DesktopAccessibilityNode {
                    target_id: BoundedText(node.target_id),
                    parent_id: node.parent_id.map(BoundedText),
                    role: BoundedText(node.role),
                    name: BoundedText(node.name),
                    description: node.description.map(BoundedText),
                    enabled: node.enabled,
                    actions: BoundedList(node.actions.into_iter().map(BoundedText).collect()),
                })
                .collect(),
        ),
        truncated: snapshot.truncated,
    };
    record.check()?;
    Ok(record)
}

#[derive(Clone)]
enum StagedInput {
    Source {
        tested_repository: Option<Value>,
        binding: Option<SourceBinding>,
    },
    Refused(Refusal),
}

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
    #[serde(default)]
    tested_repository: Option<Value>,
    #[serde(default)]
    source_binding: Option<SourceBinding>,
    input: ops::InputInput,
}

impl InputJob {
    fn new(
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::InputInput,
        tested_repository: Option<Value>,
        source_binding: Option<SourceBinding>,
    ) -> Self {
        Self {
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
            session: ctx.session_id().cloned(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            authority_sources: ctx.authority_sources().to_vec(),
            fingerprint: command.fingerprint().as_str().into(),
            tested_repository,
            source_binding,
            input: input.clone(),
        }
    }

    fn context(
        &self,
        store: &dyn ApplicationStore,
        operation: &OperationId,
    ) -> Option<InvocationContext> {
        let kind = PrincipalKind::parse(&self.kind)?;
        if store.info().restore_epoch != self.restore_epoch {
            return None;
        }
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
        if let Some(binding) = &self.source_binding {
            let bound = binding.context(store).ok()?.worktree()?.clone();
            ctx = ctx.bind_worktree(bound);
        }
        Some(ctx)
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
    git_source: Option<Arc<crate::services::git::GitService>>,
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
            git_source: None,
        }
    }

    pub fn with_git_source(
        mut self,
        source: Option<Arc<crate::services::git::GitService>>,
    ) -> Self {
        self.git_source = source;
        self
    }

    fn tested_source(
        &self,
        ctx: &InvocationContext,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<Value>, Failure> {
        if ctx.worktree().is_none() {
            return Ok(None);
        }
        self.git_source
            .as_ref()
            .ok_or_else(|| {
                Refusal::new(DomainErrorCode::UnsupportedCapability, "git_source", None)
            })?
            .observe_source(ctx, stop)
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(crate::ports::IdKind::Correlation))
    }

    /// Native automation can generate ordinary UI events. Trusted confirmation ingress stays
    /// closed while any desktop holder may inject them, regardless of that holder's project.
    pub fn automation_may_be_active(&self) -> bool {
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let key = canonical_key(
                ResourceScopeKind::Machine,
                None,
                crate::services::resources::DESKTOP,
            );
            let Some(resource) = snapshot.resources().resource_by_key(&key)? else {
                return Ok(false);
            };
            Ok(!snapshot.resources().holders(&resource.id, 0, 1)?.is_empty())
        })
        .unwrap_or(true)
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
        let revision = desktop_revision(&resource, holder.as_ref())?;
        let capabilities = self.platform.capabilities();
        let state = if !matches!(capabilities.input, DesktopAvailability::Available)
            && !matches!(capabilities.capture, DesktopAvailability::Available)
            && !matches!(capabilities.accessibility, DesktopAvailability::Available)
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
            accessibility: None,
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
        let capabilities = self.platform.capabilities();
        if !matches!(capabilities.input, DesktopAvailability::Available)
            && !matches!(capabilities.accessibility, DesktopAvailability::Available)
        {
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
        let capabilities = self.platform.capabilities();
        if !matches!(capabilities.input, DesktopAvailability::Available)
            && !matches!(capabilities.accessibility, DesktopAvailability::Available)
        {
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
        let allocated = self.resources.acquire_desktop(tx, ctx, &acquire)?;
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
        let (tested_repository, source_binding) = match command.prepared::<StagedInput>() {
            Some(StagedInput::Source {
                tested_repository,
                binding,
            }) => {
                if ctx.worktree().is_some()
                    && !binding
                        .as_ref()
                        .map(|b| b.validate_in(tx))
                        .transpose()?
                        .unwrap_or(false)
                {
                    return Err(
                        Refusal::new(DomainErrorCode::SourceChanged, "git_source", None).into(),
                    );
                }
                (tested_repository.clone(), binding.clone())
            }
            _ if ctx.worktree().is_some() => {
                return Err(
                    Refusal::new(DomainErrorCode::SourceChanged, "git_source", None).into(),
                );
            }
            _ => (None, None),
        };
        let job = InputJob::new(
            ctx,
            command,
            input,
            tested_repository.clone(),
            source_binding,
        );
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
            payload: json!({"desktopId":resource.id.as_str(),"leaseId":input.lease_id.as_str(),"consentId":input.consent_id.as_str(),"testedRepository":tested_repository}),
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

    fn validate_capture_consent(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        desktop_id: &Id,
    ) -> Result<(), Failure> {
        let key = canonical_key(
            ResourceScopeKind::Machine,
            None,
            crate::services::resources::DESKTOP,
        );
        let resource = tx
            .resources()
            .resource_by_key(&key)?
            .filter(|resource| resource.id == *desktop_id)
            .ok_or(StorageError::ScopeDenied)?;
        let lease = tx
            .resources()
            .holders(&resource.id, 0, 1)?
            .into_iter()
            .next()
            .ok_or(StorageError::ScopeDenied)?;
        let consent_id = lease
            .purpose
            .as_deref()
            .and_then(|purpose| purpose.strip_prefix(CONSENT_PURPOSE))
            .and_then(|id| Id::new(id).ok())
            .ok_or(StorageError::ScopeDenied)?;
        Self::validate_consent(tx, ctx, desktop_id, &lease.id, lease.fence, &consent_id)
    }

    fn validate_input(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::InputInput,
    ) -> Result<(), Failure> {
        Self::validate_consent(
            tx,
            ctx,
            &input.desktop_id,
            &input.lease_id,
            input.expected_fence.get(),
            &input.consent_id,
        )
    }

    fn validate_consent(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        desktop_id: &Id,
        lease_id: &Id,
        expected_fence: i64,
        consent_id: &Id,
    ) -> Result<(), Failure> {
        let lease = tx.resources().lease(lease_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "desktop_lease",
                Some(lease_id.clone()),
            )
        })?;
        if lease.owner != *ctx.principal()
            || lease.resource_id != *desktop_id
            || lease.session_id.as_ref() != ctx.session_id()
            || lease.project_id.as_ref() != ctx.project_id()
            || lease.phase != LeasePhase::Held
            || lease.expires_at_ms <= tx.now_ms()
            || lease.fence != expected_fence
            || lease.purpose.as_deref()
                != Some(format!("{CONSENT_PURPOSE}{}", consent_id.as_str()).as_str())
        {
            return Err(Refusal::new(
                DomainErrorCode::LeaseLost,
                "desktop_lease",
                Some(lease_id.clone()),
            )
            .into());
        }
        let approval = tx.decisions().approval(consent_id)?.ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent",
                Some(consent_id.clone()),
            )
        })?;
        let audit = tx
            .decisions()
            .human_decision(ApprovalSource::Generic, consent_id)?;
        let resume = tx.decisions().approval_resume(consent_id)?;
        let issued = resume.as_ref().and_then(|resume| {
            if resume.state != "Admitted"
                || resume.operation != "desktop.request_control"
                || resume.principal != *ctx.principal()
                || resume.request_digest != approval.content_digest
            {
                return None;
            }
            let request =
                serde_json::from_value::<ops::RequestControlInput>(resume.input.clone()).ok()?;
            let approved_ms = request.lease_ms.0.parse::<i64>().ok()?;
            let granted = lease.granted_at_ms?;
            let approved_until = granted.checked_add(approved_ms)?;
            if request.desktop_id != *desktop_id
                || lease.lease_ms != approved_ms
                || lease.requested_at_ms < approval.decided_at_ms?
                || tx.now_ms() >= approved_until
            {
                return None;
            }
            let CommandReply::Recorded { receipt, .. } =
                serde_json::from_value::<CommandReply<DomainDecision>>(resume.result.clone()?)
                    .ok()?
            else {
                return None;
            };
            let record = match receipt.decision {
                DomainDecision::Applied {
                    result: OperationResult::DesktopRequestControl { value },
                }
                | DomainDecision::NoChange {
                    result: OperationResult::DesktopRequestControl { value },
                } => value,
                _ => return None,
            };
            record.lease_id
        });
        if approval.state != "Approved"
            || approval.operation_kind != "desktop.request_control"
            || approval.target_id != *desktop_id
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
            || issued.as_ref() != Some(lease_id)
            || !approval.decided_by.as_ref().is_some_and(|id| {
                tx.actors().actor_kind(id).ok().flatten() == Some(PrincipalKind::Human)
            })
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent",
                Some(consent_id.clone()),
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
        let Some(ctx) = job.context(&*self.store, &operation.id) else {
            return InputEffectOutcome::Failed(
                "desktop intent principal, source binding or restore epoch invalid".into(),
            );
        };
        let tested_repository = match self.tested_source(&ctx, &|| false) {
            Ok(source) => source,
            Err(_) => return InputEffectOutcome::Failed("desktop tested source changed".into()),
        };
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
                        json!({"result":OperationResult::DesktopInput { value: record },"actionApplied":true,"testedRepository":tested_repository}),
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
            Err(crate::ports::desktop::DesktopFault::StaleTarget) => {
                InputEffectOutcome::Failed("desktop accessibility target changed".into())
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
        "capture:{};input:{};accessibility:{}",
        label(&capabilities.capture),
        label(&capabilities.input),
        label(&capabilities.accessibility)
    )
}

fn desktop_revision(
    resource: &StoredResource,
    holder: Option<&StoredLease>,
) -> Result<i64, StorageError> {
    if resource.fence_seq == 0 {
        return Ok(1);
    }
    resource
        .fence_seq
        .checked_mul(2)
        .and_then(|n| {
            n.checked_add(
                if holder.is_some_and(|lease| lease.phase == LeasePhase::Held) {
                    2
                } else {
                    3
                },
            )
        })
        .ok_or_else(|| StorageError::InvalidRecord("desktop revision overflow".into()))
}

impl QueryHandler for DesktopService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        if matches!(
            query.input(),
            OperationInput::Desktop(ops::Input::Inspect(_))
        ) {
            if query.schema_revision().0 < 3 {
                return Ok(None);
            }
            return match self.platform.capabilities().accessibility {
                DesktopAvailability::Available => {
                    let observed =
                        self.platform
                            .accessibility_snapshot()
                            .map_err(|fault| match fault {
                                crate::ports::desktop::DesktopFault::PermissionRequired => self
                                    .fault(
                                        DomainErrorCode::PermissionDenied,
                                        "accessibility",
                                        None,
                                    ),
                                crate::ports::desktop::DesktopFault::Unsupported => self.fault(
                                    DomainErrorCode::UnsupportedCapability,
                                    "accessibility",
                                    None,
                                ),
                                crate::ports::desktop::DesktopFault::StaleTarget => self.fault(
                                    DomainErrorCode::SourceChanged,
                                    "accessibility",
                                    None,
                                ),
                                crate::ports::desktop::DesktopFault::NativeFailure => self.fault(
                                    DomainErrorCode::ExternalFailure,
                                    "accessibility",
                                    None,
                                ),
                            })?;
                    let record = accessibility_record(observed).map_err(|_| {
                        self.fault(DomainErrorCode::ExternalFailure, "accessibility", None)
                    })?;
                    Ok(Some(Prepared::new(record)))
                }
                _ => Ok(None),
            };
        }
        let OperationInput::Desktop(ops::Input::Capture(input)) = query.input() else {
            return Ok(None);
        };
        let current = env
            .read(|snapshot| self.record(snapshot.resources()).map(|(_, record)| record))
            .map_err(|e| query_fault(e.into(), "desktop", self.correlation()))?;
        if current.desktop_id != input.desktop_id {
            return Err(self.fault(
                DomainErrorCode::NotFound,
                "desktop",
                Some(input.desktop_id.clone()),
            ));
        }
        if current.revision != input.expected_revision {
            return Err(self.fault(
                DomainErrorCode::RevisionConflict,
                "desktop",
                Some(input.desktop_id.clone()),
            ));
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let capture_ctx = ctx.clone();
            let desktop_id = input.desktop_id.clone();
            run_write(&*self.store, WriteOptions::default(), move |tx| {
                Self::validate_capture_consent(tx, &capture_ctx, &desktop_id)
                    .map_err(|_| StorageError::ScopeDenied)
            })
            .map_err(|_| self.fault(DomainErrorCode::PermissionDenied, "desktop_consent", None))?;
        }
        let tested_repository = self
            .tested_source(ctx, &|| env.stopped(self.clock.now_ms().get()))
            .map_err(|failure| query_fault(failure, "git_source", self.correlation()))?;
        let source_binding = SourceBinding::prepare(ctx, &*self.store)
            .map_err(|error| query_fault(error.into(), "git_source", self.correlation()))?;
        let png = self.platform.capture_png().map_err(|fault| match fault {
            crate::ports::desktop::DesktopFault::PermissionRequired => {
                self.fault(DomainErrorCode::PermissionDenied, "screen_capture", None)
            }
            crate::ports::desktop::DesktopFault::Unsupported => self.fault(
                DomainErrorCode::UnsupportedCapability,
                "screen_capture",
                None,
            ),
            crate::ports::desktop::DesktopFault::StaleTarget => {
                self.fault(DomainErrorCode::SourceChanged, "screen_capture", None)
            }
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
            principal_kind: ctx.principal_kind().as_str().to_string(),
            agent_session: ctx.session_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            title: "Desktop capture".into(),
            media_type: "image/png".into(),
            tags: vec!["desktop".into(), "capture".into()],
            source: serde_json::json!({"kind":"DesktopCapture","desktopId":input.desktop_id.as_str(),"testedRepository":tested_repository}),
        };
        let target = input.desktop_id.clone();
        let expected = input.expected_revision.clone();
        let binding = source_binding;
        let capture_ctx = ctx.clone();
        let capture_agent = ctx.principal_kind() == PrincipalKind::Agent;
        let (reference, ()) = self
            .artifacts
            .publish_evidence(&*self.store, evidence, &png, move |tx, _ref| {
                if binding
                    .as_ref()
                    .map(|binding| binding.validate_in(tx))
                    .transpose()?
                    .is_some_and(|valid| !valid)
                {
                    return Err(StorageError::ScopeDenied);
                }
                let key = canonical_key(
                    ResourceScopeKind::Machine,
                    None,
                    crate::services::resources::DESKTOP,
                );
                let resource = tx
                    .resources()
                    .resource_by_key(&key)?
                    .filter(|resource| resource.id == target)
                    .ok_or_else(|| StorageError::NotFound {
                        kind: "desktop",
                        id: target.as_str().into(),
                    })?;
                let holder = tx
                    .resources()
                    .holders(&resource.id, 0, 1)?
                    .into_iter()
                    .next();
                let current = rev(desktop_revision(&resource, holder.as_ref())?);
                if current != expected {
                    return Err(StorageError::conflict(
                        RevisionDimension::Control,
                        "desktop",
                        target.clone(),
                        expected.clone(),
                        current,
                    ));
                }
                if capture_agent {
                    Self::validate_capture_consent(tx, &capture_ctx, &target)
                        .map_err(|_| StorageError::ScopeDenied)?;
                }
                Ok(())
            })
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
        let (_, mut record) = self
            .record(snapshot.resources())
            .map_err(|e| query_fault(e.into(), "desktop", self.correlation()))?;
        let result = match query.input() {
            OperationInput::Desktop(ops::Input::Inspect(_)) => {
                record.accessibility = query.prepared::<DesktopAccessibilitySnapshot>().cloned();
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
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if !matches!(
            command.input(),
            OperationInput::Desktop(ops::Input::Input(_))
        ) {
            return Ok(None);
        }
        let tested_repository =
            match self.tested_source(ctx, &|| env.stopped(self.clock.now_ms().get())) {
                Ok(source) => source,
                Err(failure) => {
                    return Ok(Some(Prepared::new(StagedInput::Refused(match failure {
                        Failure::Refused(refusal) => refusal,
                        _ => Refusal::new(DomainErrorCode::SourceChanged, "git_source", None),
                    }))));
                }
            };
        let binding = match SourceBinding::prepare(ctx, &*self.store) {
            Ok(binding) => binding,
            Err(StorageError::ScopeDenied) => {
                return Ok(Some(Prepared::new(StagedInput::Refused(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "git_source",
                    None,
                )))));
            }
            Err(error) => return Err(error),
        };
        Ok(Some(Prepared::new(StagedInput::Source {
            tested_repository,
            binding,
        })))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let Some(StagedInput::Refused(refusal)) = command.prepared::<StagedInput>() {
            return decided(tx, Err(refusal.clone().into()));
        }
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
