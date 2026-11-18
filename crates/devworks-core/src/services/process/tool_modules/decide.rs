//! Decisions for cancel and install requests.
use super::*;

impl ProcessToolService {
    pub(super) fn execute_cancel(&self, operation: &OperationRecord) -> EffectOutcome {
        let Some(process_id) = operation
            .payload
            .get("processId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return EffectOutcome::Failed("invalid process cancellation target".into());
        };
        let owned = OwnedProcess {
            id: process_id.clone(),
            identity: NativeIdentity {
                pid: 0,
                birth: String::new(),
                group: None,
            },
            lease: None,
        };
        match self
            .processes
            .cancel(&owned, Duration::from_secs(1), Duration::from_secs(5))
        {
            Ok(StopOutcome::Reaped { .. }) => self.observe_cancel(operation),
            Ok(StopOutcome::Uncertain(reason)) => EffectOutcome::Unknown(reason),
            Err(failure) => EffectOutcome::Unknown(format!(
                "process cancellation record unavailable: {}",
                failure.error
            )),
        }
    }

    pub(super) fn observe_cancel(&self, operation: &OperationRecord) -> EffectOutcome {
        let Some(process_id) = operation
            .payload
            .get("processId")
            .and_then(Value::as_str)
            .and_then(|s| Id::new(s).ok())
        else {
            return EffectOutcome::Failed("invalid process cancellation target".into());
        };
        let record = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().process(&process_id)
        });
        let Ok(Some(record)) = record else {
            return EffectOutcome::Unknown("process cancellation state unavailable".into());
        };
        if !record.phase.quiesced() {
            return EffectOutcome::Unknown("owned process has not been verified stopped".into());
        }
        EffectOutcome::Completed(json!({"result":OperationResult::ProcessCancel {
            value: ProcessRecord {
                id: record.id,
                revision: rev(record.revision),
                state: if record.phase == ProcessPhase::Failed { OperationState::Failed } else { OperationState::ObservedComplete },
                owner_id: record.actor.0,
                exit_code: record.exit_code.and_then(|code| i32::try_from(code).ok()),
                output: None,
                truncated: false,
            }
        }}))
    }

    pub(super) fn decide_cancel(
        &self,
        ctx: &InvocationContext,
        input: &ops::CancelInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(process) = tx.resources().process(&input.process_id)? else {
            return self.decide_cancel_before_launch(ctx, input, tx);
        };
        if process.project_id.as_ref() != ctx.project_id()
            || (process.actor != *ctx.principal()
                && !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
        {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "process",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        }
        if input.expected_revision.get() != process.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "process",
                process.id,
                input.expected_revision.clone(),
                rev(process.revision),
            )
            .into());
        }
        let Some(target) = process.operation_id else {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "process_operation",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        };
        let operation = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "process.cancel".into(),
            schema_version: 1,
            args_digest: sha256_hex(format!("{}|{}", input.process_id.as_str(), process.revision).as_bytes()),
            payload: json!({"processId":input.process_id.as_str(),"sourceOperationId":target.0.as_str()}),
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(),
            aggregate_id: input.process_id.as_str().into(),
            aggregate_revision: process.revision + 1,
            kind: "ProcessCancelRequested".into(),
            payload: json!({"processId": input.process_id.as_str()}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: CANCEL_CONSUMER.into(),
            event_id: event,
            // Cleanup must run even when the root is paused or archived.
            root_id: None,
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 1,
            ready_at_ms: now,
            payload: json!({"processId":input.process_id.as_str()}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }

    pub(super) fn decide_cancel_before_launch(
        &self,
        ctx: &InvocationContext,
        input: &ops::CancelInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let operation_id = OperationId(input.process_id.clone());
        let operation = tx.operations().operation(&operation_id)?;
        let owner = operation
            .as_ref()
            .and_then(|op| match op.capability.as_str() {
                "process.execute" => serde_json::from_value::<ExecuteJob>(op.payload.clone())
                    .ok()
                    .map(|job| (job.project, job.actor)),
                "process.install_project_dependencies" => {
                    serde_json::from_value::<InstallJob>(op.payload.clone())
                        .ok()
                        .map(|job| (job.project, job.actor))
                }
                _ => None,
            });
        let visible = owner.as_ref().is_some_and(|(project, actor)| {
            project.as_ref() == ctx.project_id()
                && (*actor == *ctx.principal()
                    || matches!(
                        ctx.principal_kind(),
                        PrincipalKind::Human | PrincipalKind::Host
                    ))
        });
        if !visible {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "process",
                    Some(input.process_id.clone()),
                )
                .into()),
            );
        }
        let operation = operation.expect("visible operation");
        let (_, actor) = owner.expect("visible owner");
        if input.expected_revision.get() != operation.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "process",
                input.process_id.clone(),
                input.expected_revision.clone(),
                rev(operation.revision),
            )
            .into());
        }
        if operation.state != OperationState::CancelledBeforeDispatch {
            if !crate::ports::storage::cancellable_before_dispatch(operation.state) {
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::OutcomeUnknown,
                        "process_launch",
                        Some(input.process_id.clone()),
                    )
                    .retry(RetryAdvice::RefreshAndNewRequest)
                    .into()),
                );
            }
            let mut page = tx.queue().deliveries(
                &DeliveryFilter {
                    consumer: Some(EFFECT_CONSUMER.into()),
                    operation_id: Some(operation_id.clone()),
                },
                None,
                2,
            )?;
            if page.entries.is_empty() {
                page = tx.queue().deliveries(
                    &DeliveryFilter {
                        consumer: Some(WATCH_CONSUMER.into()),
                        operation_id: Some(operation_id.clone()),
                    },
                    None,
                    2,
                )?;
            }
            if page.entries.len() != 1 {
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "process_delivery",
                        Some(input.process_id.clone()),
                    )
                    .into()),
                );
            }
            tx.queue().cancel(&page.entries[0].id)?;
        }
        let current = tx.operations().operation(&operation_id)?.ok_or_else(|| {
            StorageError::InvalidRecord("cancelled process intent disappeared".into())
        })?;
        let record = ProcessRecord {
            id: input.process_id.clone(),
            revision: rev(current.revision),
            state: current.state,
            owner_id: actor.0,
            exit_code: None,
            output: None,
            truncated: false,
        };
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::ProcessCancel { value: record },
            }),
        )
    }

    pub(super) fn decide_request_system_install(
        &self,
        ctx: &InvocationContext,
        input: &ops::RequestSystemInstallInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if input.content_digest.0 != input.package_spec.sha256.0 {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::InvalidArgument, "content_digest", None).into()),
            );
        }
        let viewer = Viewer::resolve(tx.authority(), ctx)?;
        let artifact = tx.artifacts().artifact(&input.package_spec.artifact_id)?;
        let Some(artifact) = artifact.filter(|artifact| artifact.state == ArtifactState::Live)
        else {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        };
        if !visible(tx.artifacts(), &viewer, &artifact)? {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::NotFound,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        }
        let version = tx.artifacts().version(&input.package_spec.version_id)?;
        let exact = version.is_some_and(|version| {
            version.live()
                && version.artifact_id == artifact.id
                && version.blob_digest == input.package_spec.sha256.0
                && version.byte_count == input.package_spec.byte_length.get()
                && version.media_type == input.package_spec.media_type
        });
        if !exact {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "package_spec",
                    Some(input.package_spec.artifact_id.clone()),
                )
                .into()),
            );
        }
        let scope = ctx
            .project_id()
            .cloned()
            .map_or(ConfigScope::Workspace, ConfigScope::Project);
        let id = tx.new_id(IdKind::Event);
        let package_spec = serde_json::to_value(&input.package_spec)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        let reviewed = json!({
            "packageSpec": package_spec,
            "contentDigest": input.content_digest.0,
            "reason": input.reason.0,
            "effect": "Human approval records consent; no system install is executed by this request"
        });
        let scope_digest = sha256_hex(
            json!({
                "scope": format!("{scope:?}"),
                "target": artifact.id.as_str(),
                "targetRevision": artifact.revision,
                "contentDigest": input.content_digest.0,
                "restoreEpoch": tx.store().restore_epoch,
            })
            .to_string()
            .as_bytes(),
        );
        let proposal = StoredApproval {
            id: id.clone(),
            scope,
            revision: 1,
            state: "Pending".into(),
            operation_kind: "process.request_system_install".into(),
            target_id: artifact.id.clone(),
            target_revision: artifact.revision,
            content_digest: input.content_digest.0.clone(),
            scope_digest,
            summary: input.reason.0.clone(),
            artifact: Some(package_spec),
            payload: json!({"content":null,"reviewed":reviewed}),
            author: ctx.principal().clone(),
            author_kind: ctx.principal_kind(),
            created_at_ms: tx.now_ms(),
            decided_by: None,
            decided_at_ms: None,
            rationale: None,
            restore_epoch: tx.store().restore_epoch,
        };
        tx.decisions().put_approval(&proposal, None)?;
        crate::services::notifications::notify_approval(
            tx,
            crate::ports::storage::ApprovalSource::Generic,
            &proposal.id,
            proposal.scope.project_id(),
            "System install requested",
            &proposal.summary,
        )?;
        let history_event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: history_event,
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            aggregate_kind: "system_install_proposal".into(),
            aggregate_id: proposal.id.as_str().into(),
            aggregate_revision: 1,
            kind: "SystemInstallRequested".into(),
            payload: json!({"proposalId":proposal.id,"contentDigest":proposal.content_digest}),
        })?;
        let value = ProposalRecord {
            id,
            revision: rev(1),
            content_digest: input.content_digest.clone(),
            state: ApprovalState::Pending,
            operation_kind: BoundedText("process.request_system_install".into()),
            target_id: artifact.id,
            target_revision: rev(artifact.revision),
            summary: input.reason.clone(),
            artifact: Some(input.package_spec.clone()),
        };
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::ProcessRequestSystemInstall { value },
            }),
        )
    }

    pub(super) fn decide_install(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::InstallProjectDependenciesInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(prepared) = command.prepared::<Result<InstallPlan, BindingRefusal>>() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::Conflict, "install_preparation", None).into()),
            );
        };
        if let Err(reason) = prepared {
            return decided(tx, Err(binding_refusal(*reason).into()));
        }
        if input.allow_scripts == Some(true) {
            let mut native_grant = false;
            for spent in command.authorization().authority_grants() {
                native_grant |= tx
                    .authority()
                    .authority_grant(&spent.grant_id)?
                    .is_some_and(|grant| grant.kind == GrantKind::NativeExecution);
            }
            if !scripts_approved(ctx, native_grant) {
                // The host stages an approval of this exact request; its replay runs it.
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::ApprovalRequired,
                        "install_scripts_review",
                        Some(input.repository_id.clone()),
                    )
                    .into()),
                );
            }
        }
        let repository = tx.projects().repository(&input.repository_id)?;
        let Some(repository) = repository.filter(|repository| repository.lifecycle != "Deleted")
        else {
            return decided(
                tx,
                Err(binding_refusal(BindingRefusal::UnknownRepository).into()),
            );
        };
        if repository.revision != input.expected_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Registry,
                "repository",
                repository.id,
                input.expected_revision.clone(),
                rev(repository.revision),
            )
            .into());
        }
        let version = tx.artifacts().version(&input.manifest.version_id)?;
        if !version.is_some_and(|version| {
            version.live()
                && version.artifact_id == input.manifest.artifact_id
                && version.blob_digest == input.manifest.sha256.0
                && version.byte_count == input.manifest.byte_length.get()
        }) {
            return decided(
                tx,
                Err(binding_refusal(BindingRefusal::ChangedManifest).into()),
            );
        }
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let job = InstallJob::from_context(ctx, command.fingerprint(), input);
        let payload = serde_json::to_value(&job)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "process.install_project_dependencies".into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(), actor: ctx.principal().clone(), project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(), operation_id: Some(operation.clone()),
            aggregate_kind: "process_tool".into(), aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1, kind: "ProjectDependenciesRequested".into(),
            payload: json!({"repositoryId":input.repository_id,"manifest":input.manifest,"profile":input.profile}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"repositoryId":input.repository_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}
