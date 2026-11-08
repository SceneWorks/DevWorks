//! Exact CLI plans for optional Docker, Podman and Apple container tooling.
//!
//! A plan is inert. The service resolves registered paths, credentials, current authority and
//! resource leases before dispatching it through the owned process supervisor. No backend is
//! substituted for another when the selected executable or connection is unavailable.
use crate::protocol::records::PortProtocol;

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
            Self::Apple => vec!["system".into(), "status".into()],
            _ => vec![
                "info".into(),
                "--format".into(),
                "{{json .ServerVersion}}".into(),
            ],
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
        Ok(vec![
            "build".into(),
            "--file".into(),
            definition.into(),
            "--tag".into(),
            tag.into(),
            context.into(),
        ])
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
                    "{}:{}/{}",
                    port.host,
                    port.container,
                    match port.protocol {
                        PortProtocol::Tcp => "tcp",
                        PortProtocol::Udp => "udp",
                    }
                ),
            ]);
        }
        for (name, value) in plan.environment {
            safe_env_name(name)?;
            args.extend(["--env".into(), format!("{name}={value}")]);
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
    ) -> Result<Vec<String>, PlanError> {
        safe_name(name)?;
        if command.is_empty() {
            return Err(PlanError::InvalidCommand);
        }
        let mut args = vec!["exec".into()];
        for (key, value) in environment {
            safe_env_name(key)?;
            args.extend(["--env".into(), format!("{key}={value}")]);
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
}

pub struct CreatePlan<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub labels: &'a [(String, String)],
    pub mounts: &'a [ResolvedMount<'a>],
    pub ports: &'a [Port<'a>],
    pub environment: &'a [(String, String)],
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

fn safe_reference(value: &str) -> Result<(), PlanError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_pull_uses_image_subcommand_and_every_stop_names_one_container() {
        assert_eq!(
            Backend::Apple.pull("alpine:latest").unwrap(),
            vec!["image", "pull", "alpine:latest"]
        );
        for backend in [Backend::Docker, Backend::Podman, Backend::Apple] {
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
        }];
        let environment = [("A".into(), "value with spaces".into())];
        let plan = CreatePlan {
            name: "dw-123",
            image: "alpine:latest",
            labels: &labels,
            mounts: &mounts,
            ports: &ports,
            environment: &environment,
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
                .any(|pair| pair == ["--publish", "1080:80/tcp"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--env", "A=value with spaces"])
        );
        assert_eq!(args.last().unwrap(), "alpine:latest");
    }
}
