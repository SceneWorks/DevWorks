//! One-shot external container effects with label-fenced reconciliation.
use super::backend::{Backend, CreatePlan, Port, ResolvedMount, ownership_labels};
use super::execution::{bound_spec, with_environment};
use super::inspection::{ObservedPhase, verified_lifecycle_target};
use super::intent::{ContainerCommand, ContainerIntent};
use super::runner::CliError;
use super::selection;
use super::service::ContainerService;
use crate::fingerprint::Fingerprint;
use crate::ports::Authorization;
use crate::ports::secrets::SessionSecrets;
use crate::ports::storage::{
    ContainerPhase, OperationRecord, ReadOptions, StorageError, StoredContainer, WriteOptions,
    read_snapshot, run_write,
};
use crate::ports::{AuthorityPort, AuthorityRequest};
use crate::protocol::OperationId;
use crate::protocol::OperationState;
use crate::protocol::records::ProcessRecord;
use crate::protocol::records::{ContainerImage, ItemState};
use crate::protocol::results::OperationResult;
use crate::registry::Mode;
use crate::services::artifacts::observed::Observed;
use crate::services::sessions::rev;
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

pub enum EffectOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
}

struct ExecutionPlan<'a> {
    backend: Backend,
    args: Vec<String>,
    environment: &'a [(String, String)],
    redactions: &'a [String],
    stdin: Option<Vec<u8>>,
    timeout: Duration,
}

impl ContainerService {
    fn selected_backend(
        &self,
        requested: &crate::protocol::Id,
        project: Option<&crate::protocol::ProjectId>,
    ) -> Result<Backend, EffectOutcome> {
        let backend = Backend::parse(requested.as_str())
            .ok_or_else(|| EffectOutcome::Failed("unknown container backend".into()))?;
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            selection::selected(snapshot.configuration(), project)
        })
        .map_err(|error| {
            EffectOutcome::Unknown(format!("container settings unavailable: {error}"))
        })?;
        if selected != backend {
            return Err(EffectOutcome::Failed(
                "selected backend changed before dispatch".into(),
            ));
        }
        Ok(backend)
    }

    fn command_context(
        &self,
        operation: &OperationRecord,
        authority: &dyn AuthorityPort,
    ) -> Result<
        (
            ContainerIntent,
            crate::context::InvocationContext,
            Authorization,
        ),
        EffectOutcome,
    > {
        let intent: ContainerIntent = serde_json::from_value(operation.payload.clone())
            .map_err(|_| EffectOutcome::Failed("invalid container intent".into()))?;
        let input = intent.command.input();
        if input.operation() != operation.capability {
            return Err(EffectOutcome::Failed(
                "container intent operation mismatch".into(),
            ));
        }
        let context = intent.context(&*self.store, &operation.id).ok_or_else(|| {
            EffectOutcome::Failed("container source or restore epoch changed".into())
        })?;
        let descriptor = crate::registry::find(&operation.capability)
            .ok_or_else(|| EffectOutcome::Failed("container descriptor unavailable".into()))?;
        let binding = descriptor
            .binding(Mode::Command)
            .ok_or_else(|| EffectOutcome::Failed("container command binding unavailable".into()))?;
        let fingerprint = Fingerprint::from_hex(intent.fingerprint())
            .ok_or_else(|| EffectOutcome::Failed("invalid container intent fingerprint".into()))?;
        let authorization = authority
            .authorize(
                &context,
                &AuthorityRequest {
                    descriptor,
                    binding,
                    effect: binding.effect,
                    action: None,
                    input: &input,
                    fingerprint: &fingerprint,
                },
            )
            .map_err(|_| {
                EffectOutcome::Failed("current container authority denied dispatch".into())
            })?;
        Ok((intent, context, authorization))
    }

    fn owned_row(
        &self,
        id: &crate::protocol::Id,
        operation: &OperationId,
    ) -> Result<StoredContainer, EffectOutcome> {
        let row = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().container(id)
        })
        .map_err(|error| EffectOutcome::Unknown(format!("container record unavailable: {error}")))?
        .ok_or_else(|| EffectOutcome::Failed("container ownership record missing".into()))?;
        if row.last_operation_id != *operation {
            return Err(EffectOutcome::Failed(
                "container operation was superseded".into(),
            ));
        }
        Ok(row)
    }

    fn settle_phase(
        &self,
        id: crate::protocol::Id,
        operation: OperationId,
        pending: ContainerPhase,
        terminal: ContainerPhase,
    ) -> Result<StoredContainer, EffectOutcome> {
        let resources = self.resources.clone();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let row = tx
                .resources()
                .container(&id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "container",
                    id: id.as_str().into(),
                })?;
            if row.last_operation_id != operation {
                return Err(StorageError::InvalidRecord(
                    "container effect superseded".into(),
                ));
            }
            if row.phase == terminal {
                return Ok(row);
            }
            if row.phase != pending {
                return Err(StorageError::InvalidRecord(
                    "container effect phase changed".into(),
                ));
            }
            let expected = row.revision;
            let mut next = row;
            next.phase = terminal;
            next.revision += 1;
            next.updated_at_ms = tx.now_ms();
            tx.resources().update_container(&next, expected)?;
            if matches!(terminal, ContainerPhase::Stopped | ContainerPhase::Failed)
                && let Some(resources) = &resources
            {
                resources.release_container_leases(tx, &next)?;
            }
            Ok(next)
        })
        .map_err(|error| {
            EffectOutcome::Unknown(format!("container phase settlement unavailable: {error}"))
        })
    }

    fn run_plan(
        &self,
        context: &crate::context::InvocationContext,
        operation: &OperationId,
        plan: ExecutionPlan<'_>,
    ) -> Result<super::runner::CliResult, EffectOutcome> {
        let spec = bound_spec(&self.bindings, plan.backend, plan.args, None).map_err(|reason| {
            EffectOutcome::Failed(format!("container executable changed: {reason:?}"))
        })?;
        let mut spec = with_environment(spec, plan.backend, plan.environment);
        spec.stdin = plan.stdin;
        let secrets: Vec<String> = plan
            .environment
            .iter()
            .map(|(_, value)| value.clone())
            .chain(plan.redactions.iter().cloned())
            .collect();
        self.runner
            .run(
                context,
                Some(operation.clone()),
                spec,
                plan.timeout,
                1_048_576,
                &secrets,
            )
            .map_err(|error| match error {
                CliError::Refused(reason) => EffectOutcome::Failed(reason),
                CliError::Unknown(reason) => EffectOutcome::Unknown(reason),
            })
    }

    fn execute_create(
        &self,
        operation: &OperationRecord,
        intent: &ContainerIntent,
        context: &crate::context::InvocationContext,
        input: &crate::operations::containers::CreateInput,
    ) -> EffectOutcome {
        let Some(id) = intent.container_id.as_ref() else {
            return EffectOutcome::Failed("container ID missing from create intent".into());
        };
        let row = match self.owned_row(id, &operation.id) {
            Ok(row) => row,
            Err(outcome) => return outcome,
        };
        if !matches!(
            row.phase,
            ContainerPhase::Creating | ContainerPhase::Created
        ) || row.project_id != input.spec.project_id
        {
            return EffectOutcome::Failed("container creation phase or project changed".into());
        }
        if row.phase == ContainerPhase::Created {
            let observed = match self.inspect_live(context, &row) {
                Ok(observed) if observed.phase == ObservedPhase::Stopped => observed,
                _ => return EffectOutcome::Unknown("settled create needs owner inspection".into()),
            };
            let Some(backend) = Backend::parse(&row.engine) else {
                return EffectOutcome::Failed("unknown container backend".into());
            };
            if verified_lifecycle_target(
                backend,
                &observed,
                row.create_operation_id.0.as_str(),
                row.project_id.0.as_str(),
                &row.owner_kind,
            )
            .is_err()
            {
                return EffectOutcome::Unknown("settled create ownership changed".into());
            }
            return EffectOutcome::Completed(json!({"result":OperationResult::ContainersCreate {
                value: self.container_record(&row, &observed),
            }}));
        }
        let backend = match Backend::parse(&row.engine) {
            Some(Backend::Docker) => Backend::Docker,
            Some(Backend::Podman) => Backend::Podman,
            _ => {
                return EffectOutcome::Failed(
                    "backend create is not qualified for safe shutdown".into(),
                );
            }
        };
        let selected = read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            selection::selected(snapshot.configuration(), Some(&row.project_id))
        });
        if !matches!(selected, Ok(engine) if engine == backend) {
            return EffectOutcome::Failed("selected backend changed before create".into());
        }
        let mut expected_resources = BTreeSet::new();
        for resource in &input.spec.resource_ids.0 {
            expected_resources.insert(resource.0.as_str().to_ascii_lowercase());
        }
        for port in &input.spec.ports.0 {
            let protocol = match port.protocol {
                crate::protocol::records::PortProtocol::Tcp => "tcp",
                crate::protocol::records::PortProtocol::Udp => "udp",
            };
            expected_resources.insert(format!("hostport-{protocol}-{}", port.host_port.get()));
        }
        let lease_ids = row.resource_lease_ids.clone();
        let project = row.project_id.clone();
        let holder = format!("session:{}", row.id.as_str());
        let leases_valid = run_write(&*self.store, WriteOptions::default(), move |tx| {
            if lease_ids.len() != expected_resources.len() {
                return Ok(false);
            }
            let mut actual = BTreeSet::new();
            for lease_id in &lease_ids {
                let Some(lease) = tx.resources().lease(lease_id)? else {
                    return Ok(false);
                };
                if lease.phase != crate::ports::storage::LeasePhase::Held
                    || lease.project_id.as_ref() != Some(&project)
                    || lease.holder_key != holder
                    || (lease.expires_at_ms <= tx.now_ms()
                        && !tx.resources().container_lease_in_use(lease_id)?)
                {
                    return Ok(false);
                }
                let Some(resource) = tx.resources().resource(&lease.resource_id)? else {
                    return Ok(false);
                };
                actual.insert(resource.name);
            }
            Ok(actual == expected_resources)
        });
        match leases_valid {
            Ok(true) => {}
            Ok(false) => {
                return EffectOutcome::Failed(
                    "container resource leases changed before dispatch".into(),
                );
            }
            Err(_) => {
                return EffectOutcome::Unknown("container resource leases unavailable".into());
            }
        }
        let mut sources = Vec::new();
        for mount in &input.spec.mounts.0 {
            let resolved = if matches!(
                context.principal_kind(),
                crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host
            ) {
                self.resolve_project_path(&mount.source, &row.project_id)
            } else {
                self.resolve_agent_mount_root(&mount.source, &row.project_id)
            };
            let path = match resolved {
                Some(path) => path,
                None => {
                    return EffectOutcome::Failed(
                        "container mount source is not safely registered".into(),
                    );
                }
            };
            sources.push((
                path.to_string_lossy().into_owned(),
                mount.destination.0.clone(),
                mount.read_only,
            ));
        }
        let mounts: Vec<_> = sources
            .iter()
            .map(|(source, destination, read_only)| ResolvedMount {
                source,
                destination,
                read_only: *read_only,
            })
            .collect();
        let ports: Vec<Port<'_>> = input
            .spec
            .ports
            .0
            .iter()
            .map(|port| Port {
                host: port.host_port.get() as u16,
                container: port.container_port.get() as u16,
                protocol: &port.protocol,
            })
            .collect();
        let fallback = SessionSecrets::default();
        let environment = match self.bindings.resolve_container_environment(
            &input.spec.environment.0,
            Some(&row.project_id),
            self.secrets.as_deref().unwrap_or(&fallback),
            self.store.info().store_id.0.as_str(),
        ) {
            Ok(environment) => environment,
            Err(_) => {
                return EffectOutcome::Failed(
                    "container credential or environment unavailable".into(),
                );
            }
        };
        let labels = ownership_labels(
            operation.id.0.as_str(),
            row.project_id.0.as_str(),
            &row.owner_kind,
        );
        let command: Vec<String> = input
            .spec
            .command
            .0
            .iter()
            .map(|part| part.0.clone())
            .collect();
        let env_session = if backend != Backend::Apple && !environment.is_empty() {
            let Some(writer) = &self.workload_environment else {
                return EffectOutcome::Failed("private container environment unavailable".into());
            };
            match writer.prepare_workload(&environment) {
                Ok(session) => Some(session),
                Err(reason) => return EffectOutcome::Failed(reason.into()),
            }
        } else {
            None
        };
        let args = match backend.create(&CreatePlan {
            name: id.as_str(),
            image: &input.spec.image.0,
            labels: &labels,
            mounts: &mounts,
            ports: &ports,
            environment: &environment,
            env_file: env_session.as_ref().map(|session| session.path()),
            command: &command,
        }) {
            Ok(args) => args,
            Err(_) => return EffectOutcome::Failed("container create arguments invalid".into()),
        };
        let result = match self.run_plan(
            context,
            &operation.id,
            ExecutionPlan {
                backend,
                args,
                environment: if backend == Backend::Apple {
                    &environment
                } else {
                    &[]
                },
                redactions: &environment
                    .iter()
                    .map(|(_, value)| value.clone())
                    .collect::<Vec<_>>(),
                stdin: None,
                timeout: Duration::from_secs(60),
            },
        ) {
            Ok(result) => result,
            Err(outcome) => return outcome,
        };
        if result.exit_code != Some(0) || result.truncated {
            return EffectOutcome::Unknown("container create outcome needs inspection".into());
        }
        let observed = match self.inspect_live(context, &row) {
            Ok(observed) => observed,
            Err(_) => {
                return EffectOutcome::Unknown(
                    "created container ownership could not be verified".into(),
                );
            }
        };
        if observed.phase != ObservedPhase::Stopped {
            return EffectOutcome::Unknown("created container is not observed stopped".into());
        }
        let settled = match self.settle_phase(
            id.clone(),
            operation.id.clone(),
            ContainerPhase::Creating,
            ContainerPhase::Created,
        ) {
            Ok(row) => row,
            Err(outcome) => return outcome,
        };
        EffectOutcome::Completed(json!({"result":OperationResult::ContainersCreate {
            value: self.container_record(&settled, &observed),
        }}))
    }

    fn execute_build(
        &self,
        operation: &OperationRecord,
        context: &crate::context::InvocationContext,
        input: &crate::operations::containers::BuildInput,
    ) -> EffectOutcome {
        let backend = match self.selected_backend(&input.engine_id, Some(&input.project_id)) {
            Ok(backend) => backend,
            Err(outcome) => return outcome,
        };
        let Some(artifacts) = &self.artifacts else {
            return EffectOutcome::Failed("container build artifact reader unavailable".into());
        };
        let definition =
            match artifacts.read_exact(&*self.store, context, &input.definition, 1_048_576) {
                Ok((bytes, _)) => bytes,
                Err(_) => {
                    return EffectOutcome::Failed(
                        "container definition artifact changed or is unavailable".into(),
                    );
                }
            };
        let context_path = match self.resolve_project_path(&input.context, &input.project_id) {
            Some(path) if path.is_dir() => path,
            _ => return EffectOutcome::Failed("registered container build context changed".into()),
        };
        let args = match backend.build("-", &input.tag.0, &context_path.to_string_lossy()) {
            Ok(args) => args,
            Err(_) => return EffectOutcome::Failed("container build arguments invalid".into()),
        };
        let result = match self.run_plan(
            context,
            &operation.id,
            ExecutionPlan {
                backend,
                args,
                environment: &[],
                redactions: &[],
                stdin: Some(definition),
                timeout: Duration::from_secs(600),
            },
        ) {
            Ok(result) => result,
            Err(outcome) => return outcome,
        };
        if result.exit_code != Some(0) {
            return EffectOutcome::Unknown(
                "container build outcome needs image reconciliation".into(),
            );
        }
        EffectOutcome::Completed(json!({"result":OperationResult::ContainersBuild {
            value: ContainerImage { engine_id: input.engine_id.clone(), image: input.tag.clone(), digest: None, state: ItemState::Healthy, owned: false },
        }}))
    }

    fn execute_pull(
        &self,
        operation: &OperationRecord,
        context: &crate::context::InvocationContext,
        input: &crate::operations::containers::PullInput,
    ) -> EffectOutcome {
        let backend = match self.selected_backend(&input.engine_id, context.project_id()) {
            Ok(backend) => backend,
            Err(outcome) => return outcome,
        };
        let auth = if let Some(reference) = &input.credential_ref {
            let Some(secrets) = &self.secrets else {
                return EffectOutcome::Failed(
                    "container registry credential store unavailable".into(),
                );
            };
            let credential = match self.bindings.resolve_container_credential(
                reference,
                context.project_id(),
                &**secrets,
                self.store.info().store_id.0.as_str(),
            ) {
                Ok(credential) => credential,
                Err(_) => {
                    return EffectOutcome::Failed(
                        "container registry credential unavailable".into(),
                    );
                }
            };
            let Some(auth) = &self.registry_auth else {
                return EffectOutcome::Failed(
                    "container registry authentication unavailable".into(),
                );
            };
            match auth.prepare(backend, &input.image.0, &credential) {
                Ok(session) => Some(session),
                Err(reason) => return EffectOutcome::Failed(reason.into()),
            }
        } else {
            None
        };
        let environment = auth
            .as_ref()
            .map_or_else(Vec::new, |session| session.environment());
        let redactions = auth
            .as_ref()
            .map_or_else(Vec::new, |session| session.redactions());
        let args = match backend.pull(&input.image.0) {
            Ok(args) => args,
            Err(_) => return EffectOutcome::Failed("container image reference invalid".into()),
        };
        let result = match self.run_plan(
            context,
            &operation.id,
            ExecutionPlan {
                backend,
                args,
                environment: &environment,
                redactions: &redactions,
                stdin: None,
                timeout: Duration::from_secs(600),
            },
        ) {
            Ok(result) => result,
            Err(outcome) => return outcome,
        };
        if result.exit_code != Some(0) {
            return EffectOutcome::Unknown(
                "container pull outcome needs image reconciliation".into(),
            );
        }
        EffectOutcome::Completed(json!({"result":OperationResult::ContainersPull {
            value: ContainerImage { engine_id: input.engine_id.clone(), image: input.image.clone(), digest: None, state: ItemState::Healthy, owned: false },
        }}))
    }

    fn execute_exec(
        &self,
        operation: &OperationRecord,
        context: &crate::context::InvocationContext,
        authorization: &Authorization,
        input: &crate::operations::containers::ExecInput,
    ) -> EffectOutcome {
        let row = match read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.resources().container(&input.container_id)
        }) {
            Ok(Some(row))
                if context.project_id() == Some(&row.project_id)
                    && row.revision == input.expected_revision.get()
                    && row.phase == ContainerPhase::Running =>
            {
                row
            }
            _ => {
                return EffectOutcome::Failed(
                    "container exec target changed before dispatch".into(),
                );
            }
        };
        let engine = crate::protocol::Id::new(&row.engine).expect("stored backend ID");
        let backend = match self.selected_backend(&engine, Some(&row.project_id)) {
            Ok(backend) => backend,
            Err(outcome) => return outcome,
        };
        let observed = match self.inspect_live(context, &row) {
            Ok(observed) if observed.phase == ObservedPhase::Running => observed,
            _ => {
                return EffectOutcome::Unknown(
                    "container exec ownership or running state unavailable".into(),
                );
            }
        };
        let target = match verified_lifecycle_target(
            backend,
            &observed,
            row.create_operation_id.0.as_str(),
            row.project_id.0.as_str(),
            &row.owner_kind,
        ) {
            Ok(target) => target,
            Err(_) => {
                return EffectOutcome::Failed("container lacks an immutable owned target".into());
            }
        };
        let fallback = SessionSecrets::default();
        let environment = match self.bindings.resolve_container_environment(
            &input.environment.0,
            Some(&row.project_id),
            self.secrets.as_deref().unwrap_or(&fallback),
            self.store.info().store_id.0.as_str(),
        ) {
            Ok(environment) => environment,
            Err(_) => {
                return EffectOutcome::Failed(
                    "container exec credential or environment unavailable".into(),
                );
            }
        };
        let command: Vec<String> = input.command.0.iter().map(|part| part.0.clone()).collect();
        let env_session = if backend != Backend::Apple && !environment.is_empty() {
            let Some(writer) = &self.workload_environment else {
                return EffectOutcome::Failed("private container environment unavailable".into());
            };
            match writer.prepare_workload(&environment) {
                Ok(session) => Some(session),
                Err(reason) => return EffectOutcome::Failed(reason.into()),
            }
        } else {
            None
        };
        let args = match backend.exec(
            &target,
            &command,
            &environment,
            env_session.as_ref().map(|session| session.path()),
        ) {
            Ok(args) => args,
            Err(_) => return EffectOutcome::Failed("container exec arguments invalid".into()),
        };
        let timeout_ms = input.timeout_ms.0.parse::<u64>().unwrap_or(u64::MAX);
        if timeout_ms > 3_600_000 {
            return EffectOutcome::Failed("container exec timeout exceeds host bound".into());
        }
        let result = match self.run_plan(
            context,
            &operation.id,
            ExecutionPlan {
                backend,
                args,
                environment: if backend == Backend::Apple {
                    &environment
                } else {
                    &[]
                },
                redactions: &environment
                    .iter()
                    .map(|(_, value)| value.clone())
                    .collect::<Vec<_>>(),
                stdin: None,
                timeout: Duration::from_millis(timeout_ms),
            },
        ) {
            Ok(result) => result,
            Err(outcome) => return outcome,
        };
        let Some(artifacts) = &self.artifacts else {
            return EffectOutcome::Unknown(
                "container exec output artifact owner unavailable".into(),
            );
        };
        let output = json!({
            "stdoutBase64": base64::engine::general_purpose::STANDARD.encode(&result.stdout),
            "stderrBase64": base64::engine::general_purpose::STANDARD.encode(&result.stderr),
            "truncated": result.truncated,
        });
        let bytes = match serde_json::to_vec(&output) {
            Ok(bytes) => bytes,
            Err(_) => {
                return EffectOutcome::Unknown("container exec output encoding unavailable".into());
            }
        };
        let reference = match artifacts.publish_actor_observed(&*self.store, context, authorization,
            Observed {
                project: Some(row.project_id.clone()),
                title: format!("Container {} execution", row.id.as_str()),
                media_type: "application/json".into(),
                tags: vec!["container".into(), "execution".into()],
                source: json!({"kind":"ContainerExec","containerId":row.id.as_str(),"operationId":operation.id.0.as_str()}),
                actor_audience: false,
            }, &bytes, |_, _| Ok(())) {
            Ok((reference, ())) => reference,
            Err(_) => return EffectOutcome::Unknown("container exec output publication unavailable".into()),
        };
        EffectOutcome::Completed(json!({"result":OperationResult::ContainersExec {
            value: ProcessRecord {
                id: operation.id.0.clone(), revision: rev(1), state: OperationState::ObservedComplete,
                owner_id: context.principal().0.clone(), exit_code: result.exit_code,
                output: Some(reference), truncated: result.truncated,
            },
        }}))
    }

    fn execute_lifecycle(
        &self,
        operation: &OperationRecord,
        context: &crate::context::InvocationContext,
        id: &crate::protocol::Id,
        stop: bool,
        observe_only: bool,
    ) -> EffectOutcome {
        let row = match self.owned_row(id, &operation.id) {
            Ok(row) => row,
            Err(outcome) => return outcome,
        };
        let pending = if stop {
            ContainerPhase::Stopping
        } else {
            ContainerPhase::Starting
        };
        let terminal = if stop {
            ContainerPhase::Stopped
        } else {
            ContainerPhase::Running
        };
        if row.phase == terminal {
            let Some(backend) = Backend::parse(&row.engine) else {
                return EffectOutcome::Failed("unknown container backend".into());
            };
            let observed = match self.inspect_live(context, &row) {
                Ok(observed) => observed,
                Err(_) => {
                    return EffectOutcome::Unknown(
                        "settled lifecycle needs owner inspection".into(),
                    );
                }
            };
            let desired = if stop {
                ObservedPhase::Stopped
            } else {
                ObservedPhase::Running
            };
            if observed.phase != desired
                || verified_lifecycle_target(
                    backend,
                    &observed,
                    row.create_operation_id.0.as_str(),
                    row.project_id.0.as_str(),
                    &row.owner_kind,
                )
                .is_err()
            {
                return EffectOutcome::Unknown("settled lifecycle ownership changed".into());
            }
            let value = self.container_record(&row, &observed);
            let result = if stop {
                OperationResult::ContainersStop { value }
            } else {
                OperationResult::ContainersStart { value }
            };
            return EffectOutcome::Completed(json!({"result":result}));
        }
        if row.phase != pending {
            return EffectOutcome::Failed("container lifecycle phase changed".into());
        }
        if !stop && !row.resource_lease_ids.is_empty() {
            let lease_ids = row.resource_lease_ids.clone();
            let project = row.project_id.clone();
            let holder = format!("session:{}", row.id.as_str());
            let valid = run_write(&*self.store, WriteOptions::default(), move |tx| {
                for id in &lease_ids {
                    let Some(lease) = tx.resources().lease(id)? else {
                        return Ok(false);
                    };
                    if lease.phase != crate::ports::storage::LeasePhase::Held
                        || lease.project_id.as_ref() != Some(&project)
                        || lease.holder_key != holder
                        || (lease.expires_at_ms <= tx.now_ms()
                            && !tx.resources().container_lease_in_use(id)?)
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            });
            match valid {
                Ok(true) => {}
                Ok(false) => {
                    return EffectOutcome::Failed(
                        "container resource lease lost before start".into(),
                    );
                }
                Err(_) => {
                    return EffectOutcome::Unknown("container resource lease unavailable".into());
                }
            }
        }
        let Some(backend) = Backend::parse(&row.engine) else {
            return EffectOutcome::Failed("unknown container backend".into());
        };
        let before = match self.inspect_live(context, &row) {
            Ok(observed) => observed,
            Err(_) => return EffectOutcome::Unknown("container ownership needs inspection".into()),
        };
        let target = match verified_lifecycle_target(
            backend,
            &before,
            row.create_operation_id.0.as_str(),
            row.project_id.0.as_str(),
            &row.owner_kind,
        ) {
            Ok(target) => target,
            Err(_) => {
                return EffectOutcome::Failed("container lacks an immutable owned target".into());
            }
        };
        let desired = if stop {
            ObservedPhase::Stopped
        } else {
            ObservedPhase::Running
        };
        if before.phase != desired {
            if observe_only {
                return EffectOutcome::Unknown(
                    "container lifecycle outcome is not yet observed".into(),
                );
            }
            let args = if stop {
                backend.stop(&target)
            } else {
                backend.start(&target)
            };
            let Ok(args) = args else {
                return EffectOutcome::Failed("container lifecycle target invalid".into());
            };
            let result = match self.run_plan(
                context,
                &operation.id,
                ExecutionPlan {
                    backend,
                    args,
                    environment: &[],
                    redactions: &[],
                    stdin: None,
                    timeout: Duration::from_secs(60),
                },
            ) {
                Ok(result) => result,
                Err(outcome) => return outcome,
            };
            if result.exit_code != Some(0) || result.truncated {
                return EffectOutcome::Unknown(
                    "container lifecycle outcome needs inspection".into(),
                );
            }
        }
        let after = match self.inspect_live(context, &row) {
            Ok(observed) => observed,
            Err(_) => {
                return EffectOutcome::Unknown(
                    "container lifecycle postcondition unavailable".into(),
                );
            }
        };
        if after.native_id.as_deref() != Some(target.as_str()) || after.phase != desired {
            return EffectOutcome::Unknown(
                "container identity or phase changed during lifecycle action".into(),
            );
        }
        let settled = match self.settle_phase(id.clone(), operation.id.clone(), pending, terminal) {
            Ok(row) => row,
            Err(outcome) => return outcome,
        };
        let result = if stop {
            OperationResult::ContainersStop {
                value: self.container_record(&settled, &after),
            }
        } else {
            OperationResult::ContainersStart {
                value: self.container_record(&settled, &after),
            }
        };
        EffectOutcome::Completed(json!({"result":result}))
    }

    pub fn execute_effect(
        &self,
        operation: &OperationRecord,
        authority: &dyn AuthorityPort,
    ) -> EffectOutcome {
        let (intent, context, authorization) = match self.command_context(operation, authority) {
            Ok(value) => value,
            Err(EffectOutcome::Failed(reason)) => {
                if let Ok(intent) =
                    serde_json::from_value::<ContainerIntent>(operation.payload.clone())
                    && matches!(intent.command, ContainerCommand::Create(_))
                    && let Some(id) = intent.container_id
                    && self
                        .settle_phase(
                            id,
                            operation.id.clone(),
                            ContainerPhase::Creating,
                            ContainerPhase::Failed,
                        )
                        .is_err()
                {
                    return EffectOutcome::Unknown(
                        "container create authority refusal could not release reservations".into(),
                    );
                }
                return EffectOutcome::Failed(reason);
            }
            Err(outcome) => return outcome,
        };
        let lifecycle_gate = match &intent.command {
            ContainerCommand::Create(_) => intent
                .container_id
                .as_ref()
                .map(|id| self.lifecycle_gate(id)),
            ContainerCommand::Start(input) => Some(self.lifecycle_gate(&input.container_id)),
            ContainerCommand::Stop(input) => Some(self.lifecycle_gate(&input.container_id)),
            _ => None,
        };
        let _lifecycle_guard = lifecycle_gate
            .as_ref()
            .map(|gate| gate.lock().unwrap_or_else(|p| p.into_inner()));
        let outcome = match &intent.command {
            ContainerCommand::Build(input) => self.execute_build(operation, &context, input),
            ContainerCommand::Pull(input) => self.execute_pull(operation, &context, input),
            ContainerCommand::Create(input) => {
                self.execute_create(operation, &intent, &context, input)
            }
            ContainerCommand::Start(input) => {
                self.execute_lifecycle(operation, &context, &input.container_id, false, false)
            }
            ContainerCommand::Exec(input) => {
                self.execute_exec(operation, &context, &authorization, input)
            }
            ContainerCommand::Stop(input) => {
                self.execute_lifecycle(operation, &context, &input.container_id, true, false)
            }
        };
        if let (ContainerCommand::Create(_), EffectOutcome::Failed(reason)) =
            (&intent.command, &outcome)
        {
            if let Some(id) = &intent.container_id
                && self
                    .settle_phase(
                        id.clone(),
                        operation.id.clone(),
                        ContainerPhase::Creating,
                        ContainerPhase::Failed,
                    )
                    .is_err()
            {
                return EffectOutcome::Unknown(
                    "container create refusal could not release reservations".into(),
                );
            }
            return EffectOutcome::Failed(reason.clone());
        }
        outcome
    }

    pub fn observe_effect(
        &self,
        operation: &OperationRecord,
        authority: &dyn AuthorityPort,
    ) -> EffectOutcome {
        let (intent, context, _) = match self.command_context(operation, authority) {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
        let lifecycle_gate = match &intent.command {
            ContainerCommand::Create(_) => intent
                .container_id
                .as_ref()
                .map(|id| self.lifecycle_gate(id)),
            ContainerCommand::Start(input) => Some(self.lifecycle_gate(&input.container_id)),
            ContainerCommand::Stop(input) => Some(self.lifecycle_gate(&input.container_id)),
            _ => None,
        };
        let _lifecycle_guard = lifecycle_gate
            .as_ref()
            .map(|gate| gate.lock().unwrap_or_else(|p| p.into_inner()));
        match &intent.command {
            ContainerCommand::Create(_) => {
                let Some(id) = intent.container_id.as_ref() else {
                    return EffectOutcome::Failed("container ID missing from create intent".into());
                };
                let row = match self.owned_row(id, &operation.id) {
                    Ok(row) => row,
                    Err(outcome) => return outcome,
                };
                let observed = match self.inspect_live(&context, &row) {
                    Ok(observed) => observed,
                    Err(_) => {
                        return EffectOutcome::Unknown(
                            "container create needs owner reconciliation".into(),
                        );
                    }
                };
                if observed.phase != ObservedPhase::Stopped {
                    return EffectOutcome::Unknown(
                        "container create phase not yet verified".into(),
                    );
                }
                if row.phase == ContainerPhase::Created {
                    let Some(backend) = Backend::parse(&row.engine) else {
                        return EffectOutcome::Failed("unknown container backend".into());
                    };
                    if verified_lifecycle_target(
                        backend,
                        &observed,
                        row.create_operation_id.0.as_str(),
                        row.project_id.0.as_str(),
                        &row.owner_kind,
                    )
                    .is_err()
                    {
                        return EffectOutcome::Unknown("settled create ownership changed".into());
                    }
                    return EffectOutcome::Completed(
                        json!({"result":OperationResult::ContainersCreate {
                            value: self.container_record(&row, &observed),
                        }}),
                    );
                }
                let settled = match self.settle_phase(
                    id.clone(),
                    operation.id.clone(),
                    ContainerPhase::Creating,
                    ContainerPhase::Created,
                ) {
                    Ok(row) => row,
                    Err(outcome) => return outcome,
                };
                EffectOutcome::Completed(json!({"result":OperationResult::ContainersCreate {
                    value: self.container_record(&settled, &observed),
                }}))
            }
            ContainerCommand::Start(input) => {
                self.execute_lifecycle(operation, &context, &input.container_id, false, true)
            }
            ContainerCommand::Stop(input) => {
                self.execute_lifecycle(operation, &context, &input.container_id, true, true)
            }
            _ => EffectOutcome::Unknown("container effect requires owner reconciliation".into()),
        }
    }
}
