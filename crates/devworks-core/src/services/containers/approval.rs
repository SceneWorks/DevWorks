//! The saved-operation review of an agent's `containers.create`. The person approving it sees
//! what the container would mount, publish and run: every mount source with the host path it
//! resolves to and its read-only flag, every published port with the address it binds, and the
//! image, command, environment and resources. The approval still binds the exact request digest;
//! this review only adds what the owner currently resolves for it.
use super::service::ContainerService;
use crate::admission::DecodedRequest;
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::{OperationInput, containers as ops};
use crate::ports::Preparation;
use crate::ports::storage::{ConfigScope, ProjectRead, WriteTransaction};
use crate::protocol::DomainErrorCode;
use crate::protocol::records::{ContainerSpec, PortProtocol};
use crate::services::approvals::OwnerPreview;
use crate::services::sessions::{Failure, Refusal};
use serde_json::{Value, json};

/// The address a published port binds: loopback unless the request asks for every interface.
pub const LOOPBACK_BIND: &str = "127.0.0.1";
pub const WIDER_BIND: &str = "all host interfaces";

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<crate::protocol::Id>) -> Failure {
    Refusal::new(code, kind, id).into()
}

fn create_spec(request: &DecodedRequest) -> Result<&ContainerSpec, Failure> {
    match &request.input {
        OperationInput::Containers(ops::Input::Create(input)) => Ok(&input.spec),
        _ => Err(refuse(
            DomainErrorCode::UnsupportedCapability,
            "container_approval",
            None,
        )),
    }
}

/// The current project and the registered repositories its mounts name. A changed project or a
/// moved, re-registered or deleted mount repository makes the reviewed preview stale.
fn binding(
    projects: &dyn ProjectRead,
    ctx: &InvocationContext,
    spec: &ContainerSpec,
) -> Result<(i64, Value), Failure> {
    if ctx.project_id() != Some(&spec.project_id) {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            "container_project",
            Some(spec.project_id.0.clone()),
        ));
    }
    let project = projects
        .project(&spec.project_id)?
        .filter(|p| p.lifecycle == "Active")
        .ok_or_else(|| {
            refuse(
                DomainErrorCode::NotFound,
                "project",
                Some(spec.project_id.0.clone()),
            )
        })?;
    let mut roots = Vec::new();
    for mount in &spec.mounts.0 {
        let repository = projects.repository(&mount.source.root_id)?;
        roots.push(json!({
            "rootId": mount.source.root_id,
            "repository": repository.map(|r| json!({
                "location": r.location,
                "revision": r.revision,
                "lifecycle": r.lifecycle,
                "primary": r.primary,
            })),
        }));
    }
    Ok((
        project.revision,
        json!({
            "projectRevision": project.revision,
            "projectScopeRevision": project.scope_revision,
            "mountRoots": roots,
        }),
    ))
}

/// The reviewed content: the exact create request in the terms a person checks before approving.
/// `host_paths` holds the host path each mount source resolves to, in mount order.
pub(crate) fn reviewed(spec: &ContainerSpec, host_paths: &[String]) -> Value {
    let mounts: Vec<Value> = spec
        .mounts
        .0
        .iter()
        .zip(host_paths)
        .map(|(mount, host)| {
            json!({
                "source": mount.source,
                "hostPath": host,
                "destination": mount.destination,
                "readOnly": mount.read_only,
            })
        })
        .collect();
    let ports: Vec<Value> = spec
        .ports
        .0
        .iter()
        .map(|port| {
            let wider = port.expose_beyond_loopback.unwrap_or(false);
            json!({
                "hostPort": port.host_port.get(),
                "containerPort": port.container_port.get(),
                "protocol": match port.protocol {
                    PortProtocol::Tcp => "tcp",
                    PortProtocol::Udp => "udp",
                },
                "exposeBeyondLoopback": wider,
                "bindAddress": if wider { WIDER_BIND } else { LOOPBACK_BIND },
            })
        })
        .collect();
    let environment: Vec<Value> = spec
        .environment
        .0
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "value": entry.value,
                "credentialRef": entry.credential_ref,
            })
        })
        .collect();
    json!({
        "operation": "containers.create",
        "engineId": spec.engine_id,
        "projectId": spec.project_id,
        "image": spec.image,
        "command": spec.command,
        "mounts": mounts,
        "ports": ports,
        "environment": environment,
        "resourceIds": spec.resource_ids,
    })
}

fn summary(spec: &ContainerSpec) -> String {
    let writable = spec.mounts.0.iter().filter(|m| !m.read_only).count();
    let wider = spec
        .ports
        .0
        .iter()
        .filter(|p| p.expose_beyond_loopback.unwrap_or(false))
        .count();
    format!(
        "Review container from image {}: {} mount(s), {} writable; {} published port(s), {} on all host interfaces",
        spec.image.0,
        spec.mounts.0.len(),
        writable,
        spec.ports.0.len(),
        wider
    )
}

impl ContainerService {
    /// Resolves each mount the way dispatch will. A source dispatch would refuse (a dot-directory,
    /// a path outside a member primary checkout) is refused here, before anyone is asked.
    fn reviewed_host_paths(
        &self,
        ctx: &InvocationContext,
        spec: &ContainerSpec,
    ) -> Result<Vec<String>, Failure> {
        spec.mounts
            .0
            .iter()
            .map(|mount| {
                let resolved = if matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ) {
                    self.resolve_project_path(&mount.source, &spec.project_id)
                } else {
                    self.resolve_agent_mount_root(&mount.source, &spec.project_id)
                };
                resolved
                    .map(|path| path.to_string_lossy().into_owned())
                    .ok_or_else(|| {
                        refuse(
                            DomainErrorCode::InvalidArgument,
                            "container_mount_source",
                            Some(mount.source.root_id.clone()),
                        )
                    })
            })
            .collect()
    }

    pub(crate) fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let spec = create_spec(request)?;
        let (revision, bound) =
            env.read(|snapshot| Ok(binding(snapshot.projects(), ctx, spec)))??;
        let host_paths = self.reviewed_host_paths(ctx, spec)?;
        Ok(OwnerPreview {
            scope: ConfigScope::Project(spec.project_id.clone()),
            target_id: spec.project_id.0.clone(),
            target_revision: revision,
            summary: summary(spec),
            reviewed: reviewed(spec, &host_paths),
            binding: bound,
        })
    }

    /// Rechecks the project and mount repositories inside the deciding transaction. It does no
    /// filesystem I/O; dispatch resolves every mount again before the engine runs.
    pub(crate) fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        let spec = create_spec(request)?;
        let (revision, bound) = binding(tx.projects(), ctx, spec)?;
        if preview.scope != ConfigScope::Project(spec.project_id.clone())
            || preview.target_id != spec.project_id.0
            || preview.target_revision != revision
            || preview.binding != bound
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "container_approval",
                Some(spec.project_id.0.clone()),
            ));
        }
        Ok(())
    }
}
