//! One container domain owner for selected engine discovery and lifecycle operations.
use super::backend::Backend;
use super::backend::ownership_matches;
use super::execution::bound_spec;
use super::inspection::{Inspected, ObservedPhase, parse, verified_lifecycle_target};
use super::probe::{Availability, HostPlatform, Probe, probe};
use super::runner::ContainerRunner;
use super::selection;
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, containers as ops};
use crate::ports::storage::{ApplicationStore, ReadSnapshot, StoredContainer, read_snapshot};
use crate::ports::{IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::ProjectId;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    ContainerCapability, ContainerRecord, Diagnostic, ItemState, RegisteredPath, Severity,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::artifacts::observed::Observed;
use crate::services::process::tools::{BindingRefusal, ToolBindings};
use crate::services::sessions::{Refusal, query_fault, rev};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

#[derive(Clone)]
struct PreparedInspect {
    revision: i64,
    observed: Inspected,
}

#[derive(Clone)]
struct PreparedLogs {
    revision: i64,
    reference: ArtifactRef,
}

pub struct ContainerService {
    pub(super) store: Arc<dyn ApplicationStore>,
    pub(super) runner: Arc<ContainerRunner>,
    pub(super) bindings: ToolBindings,
    pub(super) artifacts: Option<Arc<crate::services::artifacts::ArtifactService>>,
    pub(super) secrets: Option<Arc<dyn crate::ports::secrets::SecretStore>>,
    pub(super) resources: Option<Arc<crate::services::resources::ResourceService>>,
    pub(super) registry_auth: Option<Arc<dyn super::registry_auth::RegistryAuth>>,
    host: HostPlatform<'static>,
    pub(super) ids: Arc<dyn IdSource>,
    lifecycle_gates: Mutex<HashMap<Id, Weak<Mutex<()>>>>,
}

impl ContainerService {
    pub(super) fn lifecycle_gate(&self, id: &Id) -> Arc<Mutex<()>> {
        let mut gates = self
            .lifecycle_gates
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(gate) = gates.get(id).and_then(Weak::upgrade) {
            return gate;
        }
        if gates.len() > 1024 {
            gates.retain(|_, gate| gate.strong_count() > 0);
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(id.clone(), Arc::downgrade(&gate));
        gate
    }

    /// Static host grants and current project member repositories are valid container roots.
    /// Repository IDs remain usable when projects or repositories are registered after boot.
    pub(super) fn resolve_project_path(
        &self,
        path: &RegisteredPath,
        project: &ProjectId,
    ) -> Option<PathBuf> {
        match self.bindings.resolve_registered_path(path, Some(project)) {
            Ok(resolved) => return Some(resolved),
            Err(BindingRefusal::UnknownRoot) => {}
            Err(_) => return None,
        }
        let (repository, members) = read_snapshot(
            &*self.store,
            &crate::ports::storage::ReadOptions::default(),
            |snapshot| {
                Ok((
                    snapshot.projects().repository(&path.root_id)?,
                    snapshot.projects().repository_projects(&path.root_id)?,
                ))
            },
        )
        .ok()?;
        let repository = repository?;
        if !repository.primary || repository.lifecycle == "Deleted" || !members.contains(project) {
            return None;
        }
        let root = PathBuf::from(repository.location);
        if !root.is_absolute() || root.canonicalize().ok()? != root || !root.is_dir() {
            return None;
        }
        let resolved = root.join(&path.relative.0).canonicalize().ok()?;
        resolved.starts_with(&root).then_some(resolved)
    }
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
            artifacts: None,
            secrets: None,
            resources: None,
            registry_auth: None,
            host,
            ids,
            lifecycle_gates: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_artifacts(
        mut self,
        artifacts: Arc<crate::services::artifacts::ArtifactService>,
    ) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn with_secrets(mut self, secrets: Arc<dyn crate::ports::secrets::SecretStore>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn with_resources(
        mut self,
        resources: Arc<crate::services::resources::ResourceService>,
    ) -> Self {
        self.resources = Some(resources);
        self
    }

    pub fn with_registry_auth(mut self, auth: Arc<dyn super::registry_auth::RegistryAuth>) -> Self {
        self.registry_auth = Some(auth);
        self
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
            Availability::Healthy if probe.backend == Backend::Apple => ItemState::Unavailable,
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
        if probe.backend == Backend::Apple {
            detail.push_str("; lifecycle unavailable: exact owned-only stop is unqualified");
        }
        let diagnostic = Diagnostic {
            id: self.ids.next_id(IdKind::Diagnostic),
            severity: if matches!(state, ItemState::Healthy) {
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
        let operational = matches!(state, ItemState::Healthy);
        ContainerCapability {
            engine_id: id,
            state,
            capabilities: BoundedList(if operational {
                [
                    "containers.build",
                    "containers.pull",
                    "containers.create",
                    "containers.start",
                    "containers.exec",
                    "containers.inspect",
                    "containers.logs",
                    "containers.stop",
                ]
                .into_iter()
                .map(|name| BoundedText(name.into()))
                .collect()
            } else {
                Vec::new()
            }),
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

    pub(super) fn inspect_live(
        &self,
        ctx: &InvocationContext,
        row: &StoredContainer,
    ) -> Result<Inspected, DomainFault> {
        self.inspect_live_with_timeout(ctx, row, Duration::from_secs(10))
    }

    pub(super) fn inspect_live_with_timeout(
        &self,
        ctx: &InvocationContext,
        row: &StoredContainer,
        timeout: Duration,
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
            .run(ctx, None, spec, timeout, 1_048_576, &[])
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

    pub(super) fn container_record(
        &self,
        row: &StoredContainer,
        observed: &Inspected,
    ) -> ContainerRecord {
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
            OperationInput::Containers(ops::Input::Logs(input)) => {
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
                    return Err(self.fault(DomainErrorCode::NotFound, Some(row.id)));
                }
                let backend = Backend::parse(&row.engine).ok_or_else(|| {
                    self.fault(DomainErrorCode::UnsupportedCapability, Some(row.id.clone()))
                })?;
                let observed = self.inspect_live(ctx, &row)?;
                let target = verified_lifecycle_target(
                    backend,
                    &observed,
                    row.create_operation_id.0.as_str(),
                    row.project_id.0.as_str(),
                    &row.owner_kind,
                )
                .map_err(|_| self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())))?;
                let args = backend.logs(&target).map_err(|_| {
                    self.fault(DomainErrorCode::InvalidArgument, Some(row.id.clone()))
                })?;
                let spec = bound_spec(&self.bindings, backend, args, None).map_err(|_| {
                    self.fault(DomainErrorCode::UnsupportedCapability, Some(row.id.clone()))
                })?;
                let after = input.after_bytes.get() as u64;
                let max = input.max_bytes.get() as usize;
                let deadline = Instant::now() + Duration::from_secs(10);
                let (first, stdout_total, stderr_total) = self
                    .runner
                    .run_window(
                        ctx,
                        None,
                        spec.clone(),
                        deadline.saturating_duration_since(Instant::now()),
                        (after, 0, max),
                        &[],
                    )
                    .map_err(|_| {
                        self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone()))
                    })?;
                if first.exit_code != Some(0) {
                    return Err(self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone())));
                }
                // Define one byte cursor over stdout followed by stderr. A deep stderr page
                // needs a second bounded read once the stdout boundary is known.
                let total_bytes = stdout_total.saturating_add(stderr_total);
                let mut bytes = if after < stdout_total {
                    first.stdout
                } else if after == stdout_total {
                    first.stderr.clone()
                } else if after >= total_bytes {
                    Vec::new()
                } else {
                    let (second, _, _) = self
                        .runner
                        .run_window(
                            ctx,
                            None,
                            spec,
                            deadline.saturating_duration_since(Instant::now()),
                            (u64::MAX, after - stdout_total, max),
                            &[],
                        )
                        .map_err(|_| {
                            self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone()))
                        })?;
                    if second.exit_code != Some(0) {
                        return Err(
                            self.fault(DomainErrorCode::OutcomeUnknown, Some(row.id.clone()))
                        );
                    }
                    second.stderr
                };
                if after < stdout_total && bytes.len() < max {
                    bytes.extend_from_slice(
                        &first.stderr[..(max - bytes.len()).min(first.stderr.len())],
                    );
                }
                let Some(artifacts) = &self.artifacts else {
                    return Err(
                        self.fault(DomainErrorCode::UnsupportedCapability, Some(row.id.clone()))
                    );
                };
                let offset = after.min(total_bytes);
                let end = offset.saturating_add(bytes.len() as u64);
                let (reference, ()) = artifacts.publish_observed(&*self.store, ctx, query.authorization(),
                    Observed {
                        project: Some(row.project_id.clone()),
                        title: format!("Container {} logs", row.id.as_str()),
                        media_type: "text/plain; charset=utf-8".into(),
                        tags: vec!["container".into(), "logs".into()],
                        source: serde_json::json!({"kind":"ContainerLogs","containerId":row.id.as_str(),"engine":row.engine,"offset":offset,"end":end}),
                    }, &bytes, |_, _| Ok(()))
                    .map_err(|_| self.fault(DomainErrorCode::ViewUnavailable, Some(row.id.clone())))?;
                Ok(Some(Prepared::new(PreparedLogs {
                    revision: row.revision,
                    reference,
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
            OperationInput::Containers(ops::Input::Logs(input)) => {
                let row = self.visible_container(snapshot, ctx, &input.container_id)?;
                let prepared = query
                    .prepared::<PreparedLogs>()
                    .ok_or_else(|| self.fault(DomainErrorCode::Conflict, Some(row.id.clone())))?;
                if row.revision != prepared.revision {
                    return Err(self.fault(DomainErrorCode::SourceChanged, Some(row.id)));
                }
                OperationResult::ContainersLogs {
                    value: prepared.reference.clone(),
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
