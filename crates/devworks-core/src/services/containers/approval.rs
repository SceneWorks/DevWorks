//! The saved-operation review of an agent's container operations: `containers.build`, `pull`,
//! `create`, `start` and `exec`. The person approving one sees what it would build, fetch, mount,
//! publish or run:
//! - build: the build context with the host folder it resolves to, and the Dockerfile artifact
//!   (its digest, size and text);
//! - pull: the image reference, the registry it comes from and the credential reference;
//! - create: every mount source with the host path it resolves to and its read-only flag, every
//!   published port with the address it binds, and the image, command, environment and resources;
//! - start and exec: the owned container (its image, phase and ownership), and for exec the
//!   command, environment (credential-looking values redacted) and timeout.
//!
//! The approval still binds the exact request digest; this review only adds what the owner
//! currently resolves for it. The deciding transaction and an approved replay recheck the same
//! binding and refuse with `SourceChanged` when it moved.
use super::build_context::{self, ContextDigest};
use super::service::ContainerService;
use crate::admission::DecodedRequest;
use crate::authority::redact::{REDACTED, Redactor, sensitive_name};
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::{OperationInput, containers as ops};
use crate::ports::Preparation;
use crate::ports::storage::{
    ArtifactRead, ConfigScope, ProjectRead, ReadSnapshot, ResourceRead, StoredContainer,
    WriteTransaction,
};
use crate::protocol::records::{ContainerSpec, EnvironmentEntry, PortProtocol, RegisteredPath};
use crate::protocol::{DomainErrorCode, Id, ProjectId};
use crate::services::approvals::OwnerPreview;
use crate::services::approvals::owner_preview::artifact_binding;
use crate::services::sessions::{Failure, Refusal};
use serde_json::{Value, json};

/// The address a published port binds: loopback unless the request asks for every interface.
pub const LOOPBACK_BIND: &str = "127.0.0.1";
pub const WIDER_BIND: &str = "all host interfaces";
/// The largest Dockerfile shown in a build review (the build itself reads at most this much).
const DEFINITION_LIMIT: u64 = 1_048_576;

fn refuse(code: DomainErrorCode, kind: &'static str, id: Option<Id>) -> Failure {
    Refusal::new(code, kind, id).into()
}

/// A container operation a person can review before an agent runs it.
#[derive(Clone, Copy)]
pub(crate) enum Reviewable<'a> {
    Build(&'a ops::BuildInput),
    Pull(&'a ops::PullInput),
    Create(&'a ContainerSpec),
    Start(&'a ops::StartInput),
    Exec(&'a ops::ExecInput),
}
impl<'a> Reviewable<'a> {
    fn from_request(request: &'a DecodedRequest) -> Result<Self, Failure> {
        match &request.input {
            OperationInput::Containers(ops::Input::Build(input)) => Ok(Self::Build(input)),
            OperationInput::Containers(ops::Input::Pull(input)) => Ok(Self::Pull(input)),
            OperationInput::Containers(ops::Input::Create(input)) => Ok(Self::Create(&input.spec)),
            OperationInput::Containers(ops::Input::Start(input)) => Ok(Self::Start(input)),
            OperationInput::Containers(ops::Input::Exec(input)) => Ok(Self::Exec(input)),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "container_approval",
                None,
            )),
        }
    }
}

/// Reads the binding needs, from a snapshot (preparation) or the deciding transaction.
trait Source {
    fn projects(&mut self) -> &dyn ProjectRead;
    fn artifacts(&mut self) -> &dyn ArtifactRead;
    fn resources(&mut self) -> &dyn ResourceRead;
}
struct Snapshot<'a>(&'a dyn ReadSnapshot);
impl Source for Snapshot<'_> {
    fn projects(&mut self) -> &dyn ProjectRead {
        self.0.projects()
    }
    fn artifacts(&mut self) -> &dyn ArtifactRead {
        self.0.artifacts()
    }
    fn resources(&mut self) -> &dyn ResourceRead {
        self.0.resources()
    }
}
struct Transaction<'a>(&'a mut dyn WriteTransaction);
impl Source for Transaction<'_> {
    fn projects(&mut self) -> &dyn ProjectRead {
        self.0.projects()
    }
    fn artifacts(&mut self) -> &dyn ArtifactRead {
        self.0.artifacts()
    }
    fn resources(&mut self) -> &dyn ResourceRead {
        self.0.resources()
    }
}

/// What an approval is bound to: its scope, target and the owner state a person reviewed.
struct Bound {
    scope: ConfigScope,
    target_id: Id,
    target_revision: i64,
    binding: Value,
    /// The owned container a start or exec acts on.
    container: Option<StoredContainer>,
}

/// The current project. A changed project makes the reviewed preview stale.
fn project_binding(
    src: &mut dyn Source,
    ctx: &InvocationContext,
    project_id: &ProjectId,
) -> Result<(i64, Value), Failure> {
    if ctx.project_id() != Some(project_id) {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            "container_project",
            Some(project_id.0.clone()),
        ));
    }
    let project = src
        .projects()
        .project(project_id)?
        .filter(|p| p.lifecycle == "Active")
        .ok_or_else(|| {
            refuse(
                DomainErrorCode::NotFound,
                "project",
                Some(project_id.0.clone()),
            )
        })?;
    Ok((
        project.revision,
        json!({
            "projectRevision": project.revision,
            "projectScopeRevision": project.scope_revision,
        }),
    ))
}

/// The registered repository a mount or build context names. A moved, re-registered or deleted
/// repository makes the reviewed preview stale.
fn root_binding(src: &mut dyn Source, path: &RegisteredPath) -> Result<Value, Failure> {
    let repository = src.projects().repository(&path.root_id)?;
    Ok(json!({
        "rootId": path.root_id,
        "repository": repository.map(|r| json!({
            "location": r.location,
            "revision": r.revision,
            "lifecycle": r.lifecycle,
            "primary": r.primary,
        })),
    }))
}

/// The owned container a start or exec acts on, visible to the caller as dispatch checks it.
fn owned_container(
    src: &mut dyn Source,
    ctx: &InvocationContext,
    id: &Id,
    expected_revision: i64,
) -> Result<StoredContainer, Failure> {
    let row = src
        .resources()
        .container(id)?
        .filter(|row| {
            ctx.project_id() == Some(&row.project_id)
                && (row.actor == *ctx.principal()
                    || matches!(
                        ctx.principal_kind(),
                        PrincipalKind::Human | PrincipalKind::Host
                    ))
        })
        .ok_or_else(|| refuse(DomainErrorCode::NotFound, "container", Some(id.clone())))?;
    if row.revision != expected_revision {
        return Err(refuse(
            DomainErrorCode::SourceChanged,
            "container_approval",
            Some(id.clone()),
        ));
    }
    Ok(row)
}

/// The container's identity, phase and ownership as the person reviews them.
fn container_view(row: &StoredContainer) -> Value {
    json!({
        "containerId": row.id,
        "revision": row.revision,
        "engine": row.engine,
        "image": row.image,
        "phase": format!("{:?}", row.phase),
        "owner": {
            "actor": row.actor,
            "ownerKind": row.owner_kind,
            "sessionId": row.session_id,
            "projectId": row.project_id,
            "rootId": row.root_id,
            "assignmentId": row.assignment_id,
        },
    })
}

fn binding(
    src: &mut dyn Source,
    ctx: &InvocationContext,
    item: Reviewable<'_>,
) -> Result<Bound, Failure> {
    let project = |src: &mut dyn Source, id: &ProjectId| -> Result<Bound, Failure> {
        let (revision, binding) = project_binding(src, ctx, id)?;
        Ok(Bound {
            scope: ConfigScope::Project(id.clone()),
            target_id: id.0.clone(),
            target_revision: revision,
            binding,
            container: None,
        })
    };
    match item {
        Reviewable::Create(spec) => {
            let mut bound = project(src, &spec.project_id)?;
            let mut roots = Vec::new();
            for mount in &spec.mounts.0 {
                roots.push(root_binding(src, &mount.source)?);
            }
            bound.binding["mountRoots"] = Value::Array(roots);
            Ok(bound)
        }
        Reviewable::Build(input) => {
            let definition = artifact_binding(src.artifacts(), &input.definition)?;
            let mut bound = project(src, &input.project_id)?;
            bound.binding["contextRoot"] = root_binding(src, &input.context)?;
            bound.binding["definition"] = definition;
            Ok(bound)
        }
        Reviewable::Pull(_) => {
            // A pull is engine-wide; it is reviewed and bound in the caller's project.
            let id = ctx.project_id().cloned().ok_or_else(|| {
                refuse(DomainErrorCode::PermissionDenied, "container_project", None)
            })?;
            project(src, &id)
        }
        Reviewable::Start(ops::StartInput {
            container_id,
            expected_revision,
        })
        | Reviewable::Exec(ops::ExecInput {
            container_id,
            expected_revision,
            ..
        }) => {
            let row = owned_container(src, ctx, container_id, expected_revision.get())?;
            Ok(Bound {
                scope: ConfigScope::Project(row.project_id.clone()),
                target_id: row.id.clone(),
                target_revision: row.revision,
                binding: json!({ "container": container_view(&row) }),
                container: Some(row),
            })
        }
    }
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

/// A command is shown exactly, argument by argument. One the shared redaction would change (a
/// `--password=…` argument, a credential shape, a key block) cannot be shown both safely and in
/// full, so the request is refused as unreviewable rather than shown redacted or truncated. The
/// check also reads the arguments joined with spaces, as a command line: a secret flag and its
/// value in separate arguments (`--password`, `hunter2`) is caught as well.
fn reviewable_command(argv: &[crate::protocol::support::Text]) -> Result<(), Failure> {
    let redactor = Redactor::new();
    let line = argv
        .iter()
        .map(|arg| arg.0.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if redactor.redact_text(&line) != line
        || argv.iter().any(|arg| redactor.redact_text(&arg.0) != arg.0)
    {
        return Err(refuse(
            DomainErrorCode::InvalidArgument,
            "container_command_unreviewable",
            None,
        ));
    }
    Ok(())
}

/// An exec environment as a person reviews it: a value whose name marks it as a secret, or that
/// looks like a credential, is redacted; a credential reference is shown by its id only.
fn redacted_environment(entries: &[EnvironmentEntry]) -> Vec<Value> {
    let redactor = Redactor::new();
    entries
        .iter()
        .map(|entry| {
            let value = entry.value.as_ref().map(|value| {
                if sensitive_name(&entry.name.0) {
                    REDACTED.to_owned()
                } else {
                    redactor.redact_text(&value.0)
                }
            });
            json!({
                "name": entry.name,
                "value": value,
                "credentialRef": entry.credential_ref,
            })
        })
        .collect()
}

/// The registry an image reference names: its first component when that is a host (it has a
/// `.` or `:`, or is `localhost`), otherwise Docker Hub.
fn registry(image: &str) -> &str {
    match image.split_once('/') {
        Some((first, _)) if first.contains(['.', ':']) || first == "localhost" => first,
        _ => "docker.io",
    }
}

fn summary(item: Reviewable<'_>, container: Option<&StoredContainer>) -> String {
    match item {
        Reviewable::Create(spec) => {
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
        Reviewable::Build(input) => format!(
            "Review image build {}: context {} in {}, Dockerfile {} bytes",
            input.tag.0,
            input.context.relative.0,
            input.context.root_id.0,
            input.definition.byte_length.get()
        ),
        Reviewable::Pull(input) => format!(
            "Review image pull {} from {}{}",
            input.image.0,
            registry(&input.image.0),
            if input.credential_ref.is_some() {
                " with a registry credential"
            } else {
                ""
            }
        ),
        Reviewable::Start(input) => format!(
            "Review start of container {} from image {}",
            input.container_id.0,
            container.map_or("", |row| row.image.as_str())
        ),
        Reviewable::Exec(input) => format!(
            "Review command in container {} from image {}: {} argument(s), {} environment variable(s)",
            input.container_id.0,
            container.map_or("", |row| row.image.as_str()),
            input.command.0.len(),
            input.environment.0.len()
        ),
    }
}

/// The approved build context digest a recheck carries over unverified: the transaction does
/// no filesystem I/O, so the build effect recomputes it immediately before the engine runs.
fn carry_context_digest(item: Reviewable<'_>, current: &mut Bound, approved: &Value) {
    if matches!(item, Reviewable::Build(_)) {
        current.binding["contextDigest"] = approved["contextDigest"].clone();
    }
}

/// Readmission of an approved container operation: the project, repositories, Dockerfile
/// artifact or owned container must still be the ones the person reviewed. An unreviewed request
/// (no approved binding) is unaffected. Returns the approved build context digest, which the
/// build effect checks against the folder before the engine runs.
pub(crate) fn recheck_reviewed(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &crate::gateway::AdmittedCommand,
    item: Reviewable<'_>,
) -> Result<Option<String>, Failure> {
    let Some(approved) =
        crate::services::approvals::owner_preview::approved_binding(tx, ctx, command)?
    else {
        return Ok(None);
    };
    let mut current = binding(&mut Transaction(tx), ctx, item)?;
    carry_context_digest(item, &mut current, &approved);
    if current.binding != approved {
        return Err(refuse(
            DomainErrorCode::SourceChanged,
            "container_approval",
            Some(current.target_id),
        ));
    }
    Ok(approved["contextDigest"].as_str().map(str::to_owned))
}

impl ContainerService {
    /// Resolves a registered path the way dispatch will. A source dispatch would refuse (a
    /// dot-directory, a path outside a member primary checkout) is refused here, before anyone is
    /// asked.
    fn reviewed_host_path(
        &self,
        ctx: &InvocationContext,
        source: &RegisteredPath,
        project_id: &ProjectId,
        kind: &'static str,
    ) -> Result<String, Failure> {
        let resolved = if matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) {
            self.resolve_project_path(source, project_id)
        } else {
            self.resolve_agent_mount_root(source, project_id)
        };
        resolved
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| {
                refuse(
                    DomainErrorCode::InvalidArgument,
                    kind,
                    Some(source.root_id.clone()),
                )
            })
    }

    /// The Dockerfile text a build would send, shown exactly: what a person approves is what
    /// runs. A definition the shared redaction would change (a credential shape, or a key block
    /// that would cut the rest of the text) cannot be shown both safely and in full, so it is
    /// refused as unreviewable rather than shown redacted or truncated.
    fn reviewed_definition(
        &self,
        ctx: &InvocationContext,
        input: &ops::BuildInput,
    ) -> Result<String, Failure> {
        let unavailable = || {
            refuse(
                DomainErrorCode::InvalidArgument,
                "container_definition",
                Some(input.definition.artifact_id.clone()),
            )
        };
        let artifacts = self.artifacts.as_ref().ok_or_else(unavailable)?;
        let (bytes, _) = artifacts
            .read_exact(&*self.store, ctx, &input.definition, DEFINITION_LIMIT)
            .map_err(|_| unavailable())?;
        let text = String::from_utf8(bytes).map_err(|_| unavailable())?;
        if Redactor::new().redact_text(&text) != text {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "container_definition_unreviewable",
                Some(input.definition.artifact_id.clone()),
            ));
        }
        Ok(text)
    }

    fn reviewed_content(
        &self,
        ctx: &InvocationContext,
        item: Reviewable<'_>,
        container: Option<&StoredContainer>,
        context: Option<&ContextDigest>,
    ) -> Result<Value, Failure> {
        Ok(match item {
            Reviewable::Create(spec) => {
                reviewable_command(&spec.command.0)?;
                let host_paths = spec
                    .mounts
                    .0
                    .iter()
                    .map(|mount| {
                        self.reviewed_host_path(
                            ctx,
                            &mount.source,
                            &spec.project_id,
                            "container_mount_source",
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                reviewed(spec, &host_paths)
            }
            Reviewable::Build(input) => {
                let host_path = self.reviewed_host_path(
                    ctx,
                    &input.context,
                    &input.project_id,
                    "container_build_context",
                )?;
                json!({
                    "operation": "containers.build",
                    "engineId": input.engine_id,
                    "projectId": input.project_id,
                    "tag": input.tag,
                    "context": {
                        "source": input.context,
                        "hostPath": host_path,
                        "sha256": context.map(|c| c.sha256.as_str()),
                        "entries": context.map(|c| c.entries),
                        "bytes": context.map(|c| c.bytes),
                    },
                    "definition": {
                        "artifactId": input.definition.artifact_id,
                        "versionId": input.definition.version_id,
                        "sha256": input.definition.sha256,
                        "byteLength": input.definition.byte_length,
                        "mediaType": input.definition.media_type,
                        "text": self.reviewed_definition(ctx, input)?,
                    },
                })
            }
            Reviewable::Pull(input) => json!({
                "operation": "containers.pull",
                "engineId": input.engine_id,
                "image": input.image,
                "registry": registry(&input.image.0),
                "credentialRef": input.credential_ref,
            }),
            Reviewable::Start(_) => json!({
                "operation": "containers.start",
                "container": container.map(container_view),
            }),
            Reviewable::Exec(input) => {
                reviewable_command(&input.command.0)?;
                json!({
                "operation": "containers.exec",
                "container": container.map(container_view),
                "command": input.command,
                "environment": redacted_environment(&input.environment.0),
                // `containers.exec` takes no working directory: the command runs in the
                // container's own default.
                "workingDirectory": "container default",
                "timeoutMs": input.timeout_ms,
                })
            }
        })
    }

    pub(crate) fn approval_preview(
        &self,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let item = Reviewable::from_request(request)?;
        let mut bound = env.read(|snapshot| Ok(binding(&mut Snapshot(snapshot), ctx, item)))??;
        // A build is bound to every file of its context, not only the Dockerfile: a script the
        // Dockerfile copies or runs cannot change after approval unnoticed.
        let context = match item {
            Reviewable::Build(input) => {
                let host = self.reviewed_host_path(
                    ctx,
                    &input.context,
                    &input.project_id,
                    "container_build_context",
                )?;
                let context = build_context::digest(std::path::Path::new(&host)).map_err(|_| {
                    refuse(
                        DomainErrorCode::InvalidArgument,
                        "container_build_context",
                        Some(input.context.root_id.clone()),
                    )
                })?;
                bound.binding["contextDigest"] = json!(context.sha256);
                Some(context)
            }
            _ => None,
        };
        let reviewed =
            self.reviewed_content(ctx, item, bound.container.as_ref(), context.as_ref())?;
        Ok(OwnerPreview {
            scope: bound.scope,
            target_id: bound.target_id,
            target_revision: bound.target_revision,
            summary: summary(item, bound.container.as_ref()),
            reviewed,
            binding: bound.binding,
        })
    }

    /// Rechecks the bound owner state inside the deciding transaction. It does no filesystem
    /// I/O; dispatch resolves every path and reads the Dockerfile again before the engine runs.
    pub(crate) fn validate_approval_preview(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        let item = Reviewable::from_request(request)?;
        let mut current = binding(&mut Transaction(tx), ctx, item)?;
        carry_context_digest(item, &mut current, &preview.binding);
        if preview.scope != current.scope
            || preview.target_id != current.target_id
            || preview.target_revision != current.target_revision
            || preview.binding != current.binding
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "container_approval",
                Some(current.target_id),
            ));
        }
        Ok(())
    }
}
