//! Transactional container command intents. No engine command runs before the receipt commits.
use super::backend::Backend;
use super::intent::{ContainerCommand, ContainerIntent};
use super::selection;
use super::service::ContainerService;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::ports::storage::{
    ContainerPhase, NewEvent, NewOperation, NewQueueEntry, StorageError, StoredContainer,
    WriteTransaction,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared};
use crate::protocol::results::DomainDecision;
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId, ProjectId, RevisionDimension};
use crate::services::git::context::SourceBinding;
use crate::services::sessions::{Failure, Refusal, decided, rev};
use serde_json::json;
use std::collections::BTreeSet;

pub const CONSUMER: &str = "container_effects";
pub const STOP_CONSUMER: &str = "container_stops";

fn project_matches(ctx: &InvocationContext, project: &ProjectId) -> bool {
    ctx.project_id() == Some(project)
}

fn requested_backend(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    engine: &Id,
    project: Option<&ProjectId>,
) -> Result<Result<Backend, Refusal>, StorageError> {
    let Some(backend) = Backend::parse(engine.as_str()) else {
        return Ok(Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "container_engine",
            Some(engine.clone()),
        )));
    };
    let selected = selection::selected(tx.configuration(), project.or(ctx.project_id()))?;
    if selected != backend {
        return Ok(Err(Refusal::new(
            DomainErrorCode::Conflict,
            "selected_container_engine",
            Some(engine.clone()),
        )));
    }
    if backend == Backend::Apple {
        return Ok(Err(Refusal::new(
            DomainErrorCode::UnsupportedCapability,
            "apple_container_lifecycle_unqualified",
            Some(engine.clone()),
        )));
    }
    Ok(Ok(backend))
}

fn visible_container(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<Result<StoredContainer, Refusal>, StorageError> {
    let Some(row) = tx.resources().container(id)? else {
        return Ok(Err(Refusal::new(
            DomainErrorCode::NotFound,
            "container",
            Some(id.clone()),
        )));
    };
    if ctx.project_id() != Some(&row.project_id)
        || (row.actor != *ctx.principal()
            && !matches!(
                ctx.principal_kind(),
                PrincipalKind::Human | PrincipalKind::Host
            ))
    {
        return Ok(Err(Refusal::new(
            DomainErrorCode::NotFound,
            "container",
            Some(id.clone()),
        )));
    }
    Ok(Ok(row))
}

impl CommandHandler for ContainerService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if ContainerCommand::from_input(command.input()).is_none() {
            return Ok(None);
        }
        let binding = SourceBinding::prepare(ctx, &*self.store)?;
        Ok(Some(Prepared::new(binding)))
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        admitted: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(command) = ContainerCommand::from_input(admitted.input()) else {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "container_operation",
                    None,
                )
                .into()),
            );
        };
        let Some(binding) = admitted.prepared::<Option<SourceBinding>>() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::SourceChanged, "container_source", None).into()),
            );
        };
        if binding
            .as_ref()
            .map(|b| b.validate_in(tx))
            .transpose()?
            .is_some_and(|ok| !ok)
        {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::SourceChanged, "container_source", None).into()),
            );
        }
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let mut intent = ContainerIntent::new(ctx, admitted, command.clone(), binding.clone());
        let aggregate = match &command {
            ContainerCommand::Build(input) => {
                if !project_matches(ctx, &input.project_id) {
                    return decided(
                        tx,
                        Err(Refusal::new(
                            DomainErrorCode::NotFound,
                            "project",
                            Some(input.project_id.0.clone()),
                        )
                        .into()),
                    );
                }
                match requested_backend(tx, ctx, &input.engine_id, Some(&input.project_id))? {
                    Ok(_) => {}
                    Err(refusal) => return decided(tx, Err(refusal.into())),
                }
                operation.0.as_str().to_owned()
            }
            ContainerCommand::Pull(input) => {
                match requested_backend(tx, ctx, &input.engine_id, None)? {
                    Ok(_) => {}
                    Err(refusal) => return decided(tx, Err(refusal.into())),
                }
                operation.0.as_str().to_owned()
            }
            ContainerCommand::Create(input) => {
                let spec = &input.spec;
                if !project_matches(ctx, &spec.project_id) {
                    return decided(
                        tx,
                        Err(Refusal::new(
                            DomainErrorCode::NotFound,
                            "project",
                            Some(spec.project_id.0.clone()),
                        )
                        .into()),
                    );
                }
                let backend =
                    match requested_backend(tx, ctx, &spec.engine_id, Some(&spec.project_id))? {
                        Ok(backend) => backend,
                        Err(refusal) => return decided(tx, Err(refusal.into())),
                    };
                let id = tx.new_id(IdKind::Container);
                let mut requested = BTreeSet::new();
                for resource in &spec.resource_ids.0 {
                    let name = resource.0.as_str().to_ascii_lowercase();
                    if name.starts_with("hostport-tcp-") || name.starts_with("hostport-udp-") {
                        return decided(
                            tx,
                            Err(Refusal::new(
                                DomainErrorCode::InvalidArgument,
                                "container_port_namespace",
                                Some(resource.0.clone()),
                            )
                            .into()),
                        );
                    }
                    requested.insert((name, false));
                }
                for port in &spec.ports.0 {
                    let protocol = match port.protocol {
                        crate::protocol::records::PortProtocol::Tcp => "tcp",
                        crate::protocol::records::PortProtocol::Udp => "udp",
                    };
                    if !requested.insert((
                        format!("hostport-{protocol}-{}", port.host_port.get()),
                        true,
                    )) {
                        return decided(
                            tx,
                            Err(Refusal::new(
                                DomainErrorCode::InvalidArgument,
                                "duplicate_container_port",
                                None,
                            )
                            .into()),
                        );
                    }
                }
                let mut lease_ids = Vec::new();
                if !requested.is_empty() {
                    let Some(resources) = &self.resources else {
                        return decided(
                            tx,
                            Err(Refusal::new(
                                DomainErrorCode::UnsupportedCapability,
                                "container_resources",
                                None,
                            )
                            .into()),
                        );
                    };
                    let savepoint = tx.savepoint()?;
                    for (name, machine) in requested {
                        match resources.reserve_container(tx, ctx, &id, &name, machine) {
                            Ok(Some(lease)) => lease_ids.push(lease),
                            Ok(None) => {
                                tx.rollback_to(savepoint)?;
                                return decided(
                                    tx,
                                    Err(Refusal::new(
                                        DomainErrorCode::CapacityWait,
                                        "container_resource_waiting",
                                        None,
                                    )
                                    .into()),
                                );
                            }
                            Err(Failure::Refused(refusal)) => {
                                tx.rollback_to(savepoint)?;
                                return decided(tx, Err(refusal.into()));
                            }
                            Err(Failure::Storage(error)) => {
                                tx.rollback_to(savepoint)?;
                                return Err(error.into());
                            }
                        }
                    }
                    tx.release(savepoint)?;
                }
                let now = tx.now_ms();
                tx.resources().insert_container(&StoredContainer {
                    id: id.clone(),
                    engine: backend.id().into(),
                    phase: ContainerPhase::Creating,
                    image: spec.image.0.clone(),
                    actor: ctx.principal().clone(),
                    owner_kind: match ctx.principal_kind() {
                        PrincipalKind::Human => "Human",
                        PrincipalKind::Host => "Host",
                        _ => "Agent",
                    }
                    .into(),
                    session_id: ctx.session_id().cloned(),
                    project_id: spec.project_id.clone(),
                    root_id: ctx.root_id().cloned(),
                    assignment_id: ctx.assignment_id().cloned(),
                    create_operation_id: operation.clone(),
                    last_operation_id: operation.clone(),
                    resource_lease_ids: lease_ids,
                    detail: None,
                    revision: 1,
                    created_at_ms: now,
                    updated_at_ms: now,
                })?;
                intent.container_id = Some(id.clone());
                id.as_str().to_owned()
            }
            ContainerCommand::Start(input) => {
                let row = match visible_container(tx, ctx, &input.container_id)? {
                    Ok(row) => row,
                    Err(refusal) => return decided(tx, Err(refusal.into())),
                };
                if row.revision != input.expected_revision.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Control,
                        "container",
                        row.id,
                        input.expected_revision.clone(),
                        rev(row.revision),
                    )
                    .into());
                }
                if !matches!(row.phase, ContainerPhase::Created | ContainerPhase::Stopped) {
                    return decided(
                        tx,
                        Err(Refusal::new(
                            DomainErrorCode::Conflict,
                            "container_phase",
                            Some(row.id),
                        )
                        .into()),
                    );
                }
                let expected = row.revision;
                let mut next = row;
                if next.phase == ContainerPhase::Stopped && !next.resource_lease_ids.is_empty() {
                    let Some(resources) = &self.resources else {
                        return decided(
                            tx,
                            Err(Refusal::new(
                                DomainErrorCode::UnsupportedCapability,
                                "container_resources",
                                None,
                            )
                            .into()),
                        );
                    };
                    let mut names = BTreeSet::new();
                    for id in &next.resource_lease_ids {
                        let lease = tx.resources().lease(id)?.ok_or_else(|| {
                            StorageError::InvalidRecord("container lease missing".into())
                        })?;
                        let resource =
                            tx.resources()
                                .resource(&lease.resource_id)?
                                .ok_or_else(|| {
                                    StorageError::InvalidRecord("container resource missing".into())
                                })?;
                        let is_host_port = resource.name.starts_with("hostport-tcp-")
                            || resource.name.starts_with("hostport-udp-");
                        names.insert((resource.name, is_host_port));
                    }
                    let savepoint = tx.savepoint()?;
                    let mut leases = Vec::new();
                    for (name, machine) in names {
                        match resources.reserve_container(tx, ctx, &next.id, &name, machine) {
                            Ok(Some(lease)) => leases.push(lease),
                            Ok(None) => {
                                tx.rollback_to(savepoint)?;
                                return decided(
                                    tx,
                                    Err(Refusal::new(
                                        DomainErrorCode::CapacityWait,
                                        "container_resource_waiting",
                                        None,
                                    )
                                    .into()),
                                );
                            }
                            Err(Failure::Refused(refusal)) => {
                                tx.rollback_to(savepoint)?;
                                return decided(tx, Err(refusal.into()));
                            }
                            Err(Failure::Storage(error)) => {
                                tx.rollback_to(savepoint)?;
                                return Err(error.into());
                            }
                        }
                    }
                    tx.release(savepoint)?;
                    next.resource_lease_ids = leases;
                }
                next.phase = ContainerPhase::Starting;
                next.last_operation_id = operation.clone();
                next.revision += 1;
                next.updated_at_ms = tx.now_ms();
                tx.resources().update_container(&next, expected)?;
                next.id.as_str().to_owned()
            }
            ContainerCommand::Exec(input) => {
                let row = match visible_container(tx, ctx, &input.container_id)? {
                    Ok(row) => row,
                    Err(refusal) => return decided(tx, Err(refusal.into())),
                };
                if row.revision != input.expected_revision.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Control,
                        "container",
                        row.id,
                        input.expected_revision.clone(),
                        rev(row.revision),
                    )
                    .into());
                }
                if row.phase != ContainerPhase::Running {
                    return decided(
                        tx,
                        Err(Refusal::new(
                            DomainErrorCode::Conflict,
                            "container_phase",
                            Some(row.id),
                        )
                        .into()),
                    );
                }
                row.id.as_str().to_owned()
            }
            ContainerCommand::Stop(input) => {
                let row = match visible_container(tx, ctx, &input.container_id)? {
                    Ok(row) => row,
                    Err(refusal) => return decided(tx, Err(refusal.into())),
                };
                if row.revision != input.expected_revision.get() {
                    return Err(StorageError::conflict(
                        RevisionDimension::Control,
                        "container",
                        row.id,
                        input.expected_revision.clone(),
                        rev(row.revision),
                    )
                    .into());
                }
                if row.phase == ContainerPhase::Stopped {
                    return decided(
                        tx,
                        Err(Refusal::new(
                            DomainErrorCode::Conflict,
                            "container_phase",
                            Some(row.id),
                        )
                        .into()),
                    );
                }
                let expected = row.revision;
                let mut next = row;
                next.phase = ContainerPhase::Stopping;
                next.last_operation_id = operation.clone();
                next.revision += 1;
                next.updated_at_ms = tx.now_ms();
                tx.resources().update_container(&next, expected)?;
                next.id.as_str().to_owned()
            }
        };
        let payload = serde_json::to_value(&intent)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: command.input().operation().into(),
            schema_version: 1,
            args_digest: sha256_hex(payload.to_string().as_bytes()),
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "container".into(),
            aggregate_id: aggregate,
            aggregate_revision: 1,
            kind: "ContainerOperationRequested".into(),
            payload: json!({"operation":command.input().operation()}),
        })?;
        let queue_id = tx.new_id(IdKind::QueueEntry);
        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: queue_id,
            consumer: if matches!(command, ContainerCommand::Stop(_)) {
                STOP_CONSUMER
            } else {
                CONSUMER
            }
            .into(),
            event_id: event,
            root_id: if matches!(command, ContainerCommand::Stop(_)) {
                None
            } else {
                ctx.root_id().cloned()
            },
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"operationId":operation.0.as_str()}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}
