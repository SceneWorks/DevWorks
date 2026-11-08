//! One container domain owner for selected engine discovery and lifecycle operations.
use super::backend::Backend;
use super::backend::ownership_matches;
use super::execution::bound_spec;
use super::inspection::{Inspected, ObservedPhase, parse};
use super::probe::{Availability, HostPlatform, Probe, probe};
use super::runner::ContainerRunner;
use super::selection;
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, containers as ops};
use crate::ports::storage::{ApplicationStore, ReadSnapshot, StoredContainer};
use crate::ports::{IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::records::{
    ContainerCapability, ContainerRecord, Diagnostic, ItemState, Severity,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::process::tools::ToolBindings;
use crate::services::sessions::{Refusal, query_fault, rev};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
struct PreparedInspect {
    revision: i64,
    observed: Inspected,
}

pub struct ContainerService {
    pub(super) store: Arc<dyn ApplicationStore>,
    runner: Arc<ContainerRunner>,
    bindings: ToolBindings,
    host: HostPlatform<'static>,
    ids: Arc<dyn IdSource>,
}

impl ContainerService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        runner: Arc<ContainerRunner>,
        bindings: ToolBindings,
        host: HostPlatform<'static>,
        ids: Arc<dyn IdSource>,
    ) -> Self {
        Self {
            store,
            runner,
            bindings,
            host,
            ids,
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    fn fault(&self, code: DomainErrorCode, target: Option<Id>) -> DomainFault {
        Refusal::new(code, "container_engine", target).fault(self.correlation())
    }

    fn capability(&self, probe: &Probe, selected: Backend) -> ContainerCapability {
        let id = Id::new(probe.backend.id()).expect("static backend ID");
        let state = match probe.availability {
            Availability::Unsupported | Availability::Unavailable => ItemState::Unavailable,
            Availability::Healthy => ItemState::Healthy,
        };
        let mut detail = format!(
            "selected={}; status={}; {}",
            probe.backend == selected,
            match probe.availability {
                Availability::Unsupported => "unsupported",
                Availability::Unavailable => "unavailable",
                Availability::Healthy => "healthy",
            },
            probe.detail
        );
        if let Some(path) = &probe.executable {
            detail.push_str(&format!("; executable={path}"));
        }
        if let Some(version) = &probe.version {
            detail.push_str(&format!("; version={version}"));
        }
        let diagnostic = Diagnostic {
            id: self.ids.next_id(IdKind::Diagnostic),
            severity: if probe.availability == Availability::Healthy {
                Severity::Info
            } else {
                Severity::Warning
            },
            component: BoundedText("containers".into()),
            code: BoundedText("backend_status".into()),
            message: BoundedText(detail.chars().take(4096).collect()),
            correlation_id: None,
            evidence: None,
        };
        ContainerCapability {
            engine_id: id,
            state,
            // A healthy CLI connection is not evidence that the operation matrix has passed.
            capabilities: BoundedList(Vec::new()),
            diagnostics: BoundedList(vec![diagnostic]),
        }
    }

    fn visible_container(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<StoredContainer, DomainFault> {
        let row = snapshot
            .resources()
            .container(id)
            .map_err(|error| query_fault(error.into(), "container", self.correlation()))?
            .ok_or_else(|| self.fault(DomainErrorCode::NotFound, Some(id.clone())))?;
        if ctx.project_id() != Some(&row.project_id)
            || (row.actor != *ctx.principal()
                && !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
        {
            return Err(self.fault(DomainErrorCode::NotFound, Some(id.clone())));
        }
        Ok(row)
    }

    fn inspect_live(
        &self,
        ctx: &InvocationContext,
        row: &StoredContainer,
    ) -> Result<Inspected, DomainFault> {
        let backend = Backend::parse(&row.engine).ok_or_else(|| {
            self.fault(DomainErrorCode::UnsupportedCapability, Some(row.id.clone()))
        })?;
        let args = backend
            .inspect(row.id.as_str())
            .map_err(|_| self.fault(DomainErrorCode::InvalidArgument, Some(row.id.clone())))?;
        let spec = bound_spec(&self.bindings, backend, args, None).map_err(|_| {
            self.fault(DomainErrorCode::UnsupportedCapability, Some(row.id.clone()))
        })?;
        let result = self
            .runner
            .run(ctx, None, spec, Duration::from_secs(10), 1_048_576, &[])
            .map_err(|_| self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())))?;
        if result.exit_code != Some(0) || result.truncated {
            return Err(self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())));
        }
        let observed = parse(backend, &result.stdout)
            .map_err(|_| self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())))?;
        if !ownership_matches(
            &observed.labels,
            row.create_operation_id.0.as_str(),
            row.project_id.0.as_str(),
            &row.owner_kind,
        ) {
            return Err(self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())));
        }
        Ok(observed)
    }

    fn container_record(&self, row: &StoredContainer, observed: &Inspected) -> ContainerRecord {
        ContainerRecord {
            id: row.id.clone(),
            revision: rev(row.revision),
            engine_id: Id::new(&row.engine).expect("stored backend is validated"),
            owned: true,
            state: match observed.phase {
                ObservedPhase::Running => ItemState::Enabled,
                ObservedPhase::Stopped => ItemState::Disabled,
                ObservedPhase::Unknown => ItemState::Partial,
            },
            image: BoundedText(row.image.clone()),
            process_owner_id: None,
        }
    }
}

impl QueryHandler for ContainerService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        match query.input() {
            OperationInput::Containers(ops::Input::Capabilities(input)) => {
                let engines =
                    match &input.engine_id {
                        Some(id) => vec![Backend::parse(id.as_str()).ok_or_else(|| {
                            self.fault(DomainErrorCode::NotFound, Some(id.clone()))
                        })?],
                        None => vec![Backend::Docker, Backend::Podman, Backend::Apple],
                    };
                let probes = engines
                    .into_iter()
                    .map(|engine| probe(&self.runner, &self.bindings, ctx, engine, self.host))
                    .collect::<Vec<_>>();
                Ok(Some(Prepared::new(probes)))
            }
            OperationInput::Containers(ops::Input::Inspect(input)) => {
                let row = env
                    .read(|snapshot| snapshot.resources().container(&input.container_id))
                    .map_err(|error| query_fault(error.into(), "container", self.correlation()))?
                    .ok_or_else(|| {
                        self.fault(DomainErrorCode::NotFound, Some(input.container_id.clone()))
                    })?;
                if ctx.project_id() != Some(&row.project_id)
                    || (row.actor != *ctx.principal()
                        && !matches!(
                            ctx.principal_kind(),
                            PrincipalKind::Human | PrincipalKind::Host
                        ))
                {
                    return Err(
                        self.fault(DomainErrorCode::NotFound, Some(input.container_id.clone()))
                    );
                }
                let observed = self.inspect_live(ctx, &row)?;
                Ok(Some(Prepared::new(PreparedInspect {
                    revision: row.revision,
                    observed,
                })))
            }
            _ => Ok(None),
        }
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match query.input() {
            OperationInput::Containers(ops::Input::Capabilities(_)) => {
                let selected = selection::selected(snapshot.configuration(), ctx.project_id())
                    .map_err(|error| {
                        query_fault(error.into(), "container_settings", self.correlation())
                    })?;
                let probes = query
                    .prepared::<Vec<Probe>>()
                    .ok_or_else(|| self.fault(DomainErrorCode::Conflict, None))?;
                OperationResult::ContainersCapabilities {
                    value: Page {
                        items: BoundedList(
                            probes
                                .iter()
                                .map(|p| self.capability(p, selected))
                                .collect(),
                        ),
                        next_cursor: None,
                        has_more: false,
                    },
                }
            }
            OperationInput::Containers(ops::Input::Inspect(input)) => {
                let row = self.visible_container(snapshot, ctx, &input.container_id)?;
                let prepared = query
                    .prepared::<PreparedInspect>()
                    .ok_or_else(|| self.fault(DomainErrorCode::Conflict, Some(row.id.clone())))?;
                if row.revision != prepared.revision {
                    return Err(self.fault(DomainErrorCode::SourceChanged, Some(row.id)));
                }
                OperationResult::ContainersInspect {
                    value: self.container_record(&row, &prepared.observed),
                }
            }
            _ => return Err(self.fault(DomainErrorCode::UnsupportedCapability, None)),
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}
