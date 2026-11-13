//! Exact CLI plans for optional Docker, Podman and Apple container tooling.
//!
//! A plan is inert. The service resolves registered paths, credentials, current authority and
//! resource leases before dispatching it through the owned process supervisor. No backend is
//! substituted for another when the selected executable or connection is unavailable.
use crate::protocol::records::PortProtocol;
use std::collections::BTreeMap;

pub const CREATE_OPERATION_LABEL: &str = "devworks.create_operation";
pub const PROJECT_LABEL: &str = "devworks.project";
pub const OWNER_KIND_LABEL: &str = "devworks.owner_kind";

/// The external container must carry the identity recorded before its create effect ran.
/// A reused name without these exact labels is never an owned shutdown target.
pub fn ownership_labels(
    create_operation: &str,
    project: &str,
    owner_kind: &str,
) -> Vec<(String, String)> {
    vec![
        (CREATE_OPERATION_LABEL.into(), create_operation.into()),
        (PROJECT_LABEL.into(), project.into()),
        (OWNER_KIND_LABEL.into(), owner_kind.into()),
    ]
}

pub fn ownership_matches(
    observed: &BTreeMap<String, String>,
    create_operation: &str,
    project: &str,
    owner_kind: &str,
) -> bool {
    ownership_labels(create_operation, project, owner_kind)
        .iter()
        .all(|(key, value)| observed.get(key) == Some(value))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Docker,
    Podman,
    Apple,
}

impl Backend {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "docker" => Some(Self::Docker),
            "podman" => Some(Self::Podman),
            "apple" => Some(Self::Apple),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Apple => "apple",
        }
    }

    pub fn executable(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Apple => "container",
        }
    }

    pub fn supported_host(self, os: &str, arch: &str, macos_major: u32) -> bool {
        !matches!(self, Self::Apple) || (os == "macos" && arch == "aarch64" && macos_major >= 26)
    }

    pub fn version(self) -> Vec<String> {
        match self {
            Self::Apple => vec![
                "system".into(),
                "version".into(),
                "--format".into(),
                "json".into(),
            ],
            _ => vec!["--version".into()],
        }
    }

    pub fn health(self) -> Vec<String> {
        match self {
            Self::Apple => vec![
                "system".into(),
                "status".into(),
                "--format".into(),
                "json".into(),
            ],
            _ => vec!["info".into(), "--format".into(), "json".into()],
        }
    }

    pub fn build(
        self,
        definition: &str,
        tag: &str,
        context: &str,
    ) -> Result<Vec<String>, PlanError> {
        safe_reference(tag)?;
        safe_path(definition)?;
        safe_path(context)?;
        let mut args = vec!["build".into()];
        match self {
            Self::Docker => args.push("--pull".into()),
            Self::Podman => args.push("--pull=always".into()),
            Self::Apple => {}
        }
        args.extend([
            "--file".into(),
            definition.into(),
            "--tag".into(),
            tag.into(),
            context.into(),
        ]);
        Ok(args)
    }

    pub fn pull(self, image: &str) -> Result<Vec<String>, PlanError> {
        safe_reference(image)?;
        let mut args = Vec::new();
        if self == Self::Apple {
            args.push("image".into());
        }
        args.extend(["pull".into(), image.into()]);
        Ok(args)
    }

    pub fn image_tag(self, source: &str, target: &str) -> Result<Vec<String>, PlanError> {
        safe_reference(source)?;
        safe_reference(target)?;
        Ok(vec![
            "image".into(),
            "tag".into(),
            source.into(),
            target.into(),
        ])
    }

    pub fn image_id(self, image: &str) -> Result<Vec<String>, PlanError> {
        safe_reference(image)?;
        Ok(vec![
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            image.into(),
        ])
    }

    pub fn image_tags(self, image: &str) -> Result<Vec<String>, PlanError> {
        safe_reference(image)?;
        Ok(vec![
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{json .RepoTags}}".into(),
            image.into(),
        ])
    }

    pub fn create(self, plan: &CreatePlan<'_>) -> Result<Vec<String>, PlanError> {
        safe_name(plan.name)?;
        safe_reference(plan.image)?;
        let mut args = vec!["create".into(), "--name".into(), plan.name.into()];
        for (key, value) in plan.labels {
            safe_label(key, value)?;
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        for mount in plan.mounts {
            safe_path(mount.source)?;
            safe_path(mount.destination)?;
            let mut spec = format!(
                "type=bind,source={},target={}",
                mount.source, mount.destination
            );
            if mount.read_only {
                spec.push_str(",readonly");
            }
            args.extend(["--mount".into(), spec]);
        }
        for port in plan.ports {
            args.extend([
                "--publish".into(),
                format!(
                    "{}{}:{}/{}",
                    // The engine binds a bare `host:container` publish on every interface, so the
                    // default names the loopback address; only an explicit request omits it.
                    if port.expose_beyond_loopback {
                        ""
                    } else {
                        "127.0.0.1:"
                    },
                    port.host,
                    port.container,
                    match port.protocol {
                        PortProtocol::Tcp => "tcp",
                        PortProtocol::Udp => "udp",
                    }
                ),
            ]);
        }
        for (name, _) in plan.environment {
            safe_env_name(name)?;
        }
        if let Some(path) = plan.env_file {
            safe_env_file(path)?;
            args.extend(["--env-file".into(), path.into()]);
        } else if self == Self::Apple {
            for (name, _) in plan.environment {
                args.extend(["--env".into(), name.clone()]);
            }
        } else if !plan.environment.is_empty() {
            return Err(PlanError::InvalidEnvironment);
        }
        args.push(plan.image.into());
        args.extend(plan.command.iter().cloned());
        Ok(args)
    }

    pub fn start(self, name: &str) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        Ok(vec!["start".into(), name.into()])
    }

    pub fn exec(
        self,
        name: &str,
        command: &[String],
        environment: &[(String, String)],
        env_file: Option<&str>,
    ) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        if command.is_empty() {
            return Err(PlanError::InvalidCommand);
        }
        let mut args = vec!["exec".into()];
        for (key, _) in environment {
            safe_env_name(key)?;
        }
        if let Some(path) = env_file {
            safe_env_file(path)?;
            args.extend(["--env-file".into(), path.into()]);
        } else if self == Self::Apple {
            for (key, _) in environment {
                args.extend(["--env".into(), key.clone()]);
            }
        } else if !environment.is_empty() {
            return Err(PlanError::InvalidEnvironment);
        }
        args.push(name.into());
        args.extend(command.iter().cloned());
        Ok(args)
    }

    pub fn inspect(self, name: &str) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        Ok(vec!["inspect".into(), name.into()])
    }

    pub fn logs(self, name: &str) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        Ok(vec!["logs".into(), name.into()])
    }

    pub fn stop(self, name: &str) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        Ok(vec![
            "stop".into(),
            "--time".into(),
            "5".into(),
            name.into(),
        ])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    InvalidName,
    InvalidReference,
    InvalidPath,
    InvalidLabel,
    InvalidEnvironment,
    InvalidCommand,
}

pub struct ResolvedMount<'a> {
    pub source: &'a str,
    pub destination: &'a str,
    pub read_only: bool,
}

pub struct Port<'a> {
    pub host: u16,
    pub container: u16,
    pub protocol: &'a PortProtocol,
    /// Publish on every host interface instead of 127.0.0.1 only.
    pub expose_beyond_loopback: bool,
}

pub struct CreatePlan<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub labels: &'a [(String, String)],
    pub mounts: &'a [ResolvedMount<'a>],
    pub ports: &'a [Port<'a>],
    pub environment: &'a [(String, String)],
    pub env_file: Option<&'a str>,
    pub command: &'a [String],
}

fn safe_name(value: &str) -> Result<(), PlanError> {
    if value.len() > 128
        || value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        || !value.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(PlanError::InvalidName);
    }
    Ok(())
}

pub(super) fn safe_reference(value: &str) -> Result<(), PlanError> {
    if value.is_empty()
        || value.len() > 4096
        || value.starts_with('-')
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        return Err(PlanError::InvalidReference);
    }
    Ok(())
}

fn safe_path(value: &str) -> Result<(), PlanError> {
    if value.is_empty() || value.contains(',') || value.contains('\0') {
        return Err(PlanError::InvalidPath);
    }
    Ok(())
}

fn safe_label(key: &str, value: &str) -> Result<(), PlanError> {
    if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
        return Err(PlanError::InvalidLabel);
    }
    Ok(())
}

fn safe_env_name(value: &str) -> Result<(), PlanError> {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(PlanError::InvalidEnvironment);
    }
    Ok(())
}

fn safe_env_file(value: &str) -> Result<(), PlanError> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        return Err(PlanError::InvalidPath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_container_name_without_original_ownership_labels_is_never_owned() {
        let mut observed =
            BTreeMap::from_iter(ownership_labels("operation-a", "project-a", "Agent"));
        assert!(ownership_matches(
            &observed,
            "operation-a",
            "project-a",
            "Agent"
        ));
        observed.insert(CREATE_OPERATION_LABEL.into(), "operation-b".into());
        assert!(!ownership_matches(
            &observed,
            "operation-a",
            "project-a",
            "Agent"
        ));
        assert!(!ownership_matches(
            &observed,
            "operation-b",
            "project-b",
            "Agent"
        ));
    }

    #[test]
    fn apple_pull_uses_image_subcommand_and_every_stop_names_one_container() {
        assert_eq!(
            Backend::Apple.pull("alpine:latest").unwrap(),
            vec!["image", "pull", "alpine:latest"]
        );
        for backend in [Backend::Docker, Backend::Podman, Backend::Apple] {
            assert_eq!(backend.health().last().map(String::as_str), Some("json"));
            assert_eq!(
                backend.stop("dw-123").unwrap(),
                vec!["stop", "--time", "5", "dw-123"]
            );
            assert!(backend.stop("--all").is_err());
        }
    }

    #[test]
    fn create_preserves_owned_labels_mounts_and_ports_without_shell_expansion() {
        let labels = [("devworks.owner".into(), "agent-a".into())];
        let mounts = [ResolvedMount {
            source: "/tmp/a path",
            destination: "/work",
            read_only: true,
        }];
        let ports = [Port {
            host: 1080,
            container: 80,
            protocol: &PortProtocol::Tcp,
            expose_beyond_loopback: false,
        }];
        let environment = [("A".into(), "value with spaces".into())];
        let plan = CreatePlan {
            name: "dw-123",
            image: "alpine:latest",
            labels: &labels,
            mounts: &mounts,
            ports: &ports,
            environment: &environment,
            env_file: Some("/private, scoped/env"),
            command: &[],
        };
        let args = Backend::Docker.create(&plan).unwrap();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--label", "devworks.owner=agent-a"])
        );
        assert!(args.windows(2).any(|pair| pair
            == [
                "--mount",
                "type=bind,source=/tmp/a path,target=/work,readonly"
            ]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--publish", "127.0.0.1:1080:80/tcp"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env-file", "/private, scoped/env"])
        );
        assert!(!args.iter().any(|arg| arg.contains("value with spaces")));
        assert_eq!(args.last().unwrap(), "alpine:latest");
    }

    #[test]
    fn published_ports_bind_loopback_unless_the_request_widens_them() {
        let ports = [
            Port {
                host: 1080,
                container: 80,
                protocol: &PortProtocol::Tcp,
                expose_beyond_loopback: false,
            },
            Port {
                host: 5353,
                container: 53,
                protocol: &PortProtocol::Udp,
                expose_beyond_loopback: false,
            },
            Port {
                host: 8443,
                container: 443,
                protocol: &PortProtocol::Tcp,
                expose_beyond_loopback: true,
            },
        ];
        let plan = CreatePlan {
            name: "dw-123",
            image: "alpine:latest",
            labels: &[],
            mounts: &[],
            ports: &ports,
            environment: &[],
            env_file: None,
            command: &[],
        };
        for backend in [Backend::Docker, Backend::Podman, Backend::Apple] {
            let args = backend.create(&plan).unwrap();
            let published: Vec<&str> = args
                .windows(2)
                .filter(|pair| pair[0] == "--publish")
                .map(|pair| pair[1].as_str())
                .collect();
            assert_eq!(
                published,
                [
                    "127.0.0.1:1080:80/tcp",
                    "127.0.0.1:5353:53/udp",
                    "8443:443/tcp"
                ]
            );
        }
    }
}
