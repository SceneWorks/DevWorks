//! One container domain owner for selected engine discovery and lifecycle operations.
use super::backend::Backend;
use super::probe::{Availability, HostPlatform, Probe, probe};
use super::runner::ContainerRunner;
use super::selection;
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, containers as ops};
use crate::ports::storage::{ApplicationStore, ReadSnapshot};
use crate::ports::{IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::records::{ContainerCapability, Diagnostic, ItemState, Severity};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::process::tools::ToolBindings;
use crate::services::sessions::{Refusal, query_fault, rev};
use std::sync::Arc;

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
}

impl QueryHandler for ContainerService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let OperationInput::Containers(ops::Input::Capabilities(input)) = query.input() else {
            return Ok(None);
        };
        let engines = match &input.engine_id {
            Some(id) => vec![
                Backend::parse(id.as_str())
                    .ok_or_else(|| self.fault(DomainErrorCode::NotFound, Some(id.clone())))?,
            ],
            None => vec![Backend::Docker, Backend::Podman, Backend::Apple],
        };
        let probes = engines
            .into_iter()
            .map(|engine| probe(&self.runner, &self.bindings, ctx, engine, self.host))
            .collect::<Vec<_>>();
        Ok(Some(Prepared::new(probes)))
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Containers(ops::Input::Capabilities(_)) = query.input() else {
            return Err(self.fault(DomainErrorCode::UnsupportedCapability, None));
        };
        let selected = selection::selected(snapshot.configuration(), ctx.project_id())
            .map_err(|error| query_fault(error.into(), "container_settings", self.correlation()))?;
        let probes = query
            .prepared::<Vec<Probe>>()
            .ok_or_else(|| self.fault(DomainErrorCode::Conflict, None))?;
        let value = Page {
            items: BoundedList(
                probes
                    .iter()
                    .map(|p| self.capability(p, selected))
                    .collect(),
            ),
            next_cursor: None,
            has_more: false,
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result: OperationResult::ContainersCapabilities { value },
        })
    }
}
