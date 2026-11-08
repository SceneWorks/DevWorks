//! Private registry auth files scoped to one native container CLI pull.
use base64::Engine as _;
use devworks_core::services::containers::backend::Backend;
use devworks_core::services::containers::registry_auth::{
    RegistryAuth, RegistryAuthSession, WorkloadEnvironment, WorkloadEnvironmentSession,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::{Builder, TempDir};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    username: String,
    password: String,
}

pub struct NativeRegistryAuth {
    root: PathBuf,
}

struct NativeAuthSession {
    _directory: TempDir,
    environment: Vec<(String, String)>,
    redactions: Vec<String>,
}

struct NativeEnvironmentSession {
    _directory: TempDir,
    path: String,
}

impl WorkloadEnvironmentSession for NativeEnvironmentSession {
    fn path(&self) -> &str {
        &self.path
    }
}

impl RegistryAuthSession for NativeAuthSession {
    fn environment(&self) -> Vec<(String, String)> {
        self.environment.clone()
    }
    fn redactions(&self) -> Vec<String> {
        self.redactions.clone()
    }
}

impl NativeRegistryAuth {
    /// The app holds the one store write lock before clearing auth files left by a crash.
    pub fn new(root: PathBuf) -> Result<Self, &'static str> {
        fs::create_dir_all(&root).map_err(|_| "registry auth root unavailable")?;
        let metadata = fs::symlink_metadata(&root).map_err(|_| "registry auth root unavailable")?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err("registry auth root is not a directory");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                    .map_err(|_| "registry auth root permissions unavailable")?;
            }
        }
        for entry in fs::read_dir(&root).map_err(|_| "registry auth recovery unavailable")? {
            let entry = entry.map_err(|_| "registry auth recovery unavailable")?;
            if !["dw-auth-", "dw-env-"]
                .iter()
                .any(|prefix| entry.file_name().to_string_lossy().starts_with(prefix))
            {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|_| "registry auth recovery unavailable")?
                .is_dir()
            {
                return Err("registry auth recovery found an unexpected entry");
            }
            fs::remove_dir_all(entry.path()).map_err(|_| "registry auth recovery unavailable")?;
        }
        Ok(Self { root })
    }
}

impl WorkloadEnvironment for NativeRegistryAuth {
    fn prepare_workload(
        &self,
        environment: &[(String, String)],
    ) -> Result<Box<dyn WorkloadEnvironmentSession>, &'static str> {
        if environment.iter().any(|(name, value)| {
            name.contains(['\n', '\r', '=', '\0']) || value.contains(['\n', '\r', '\0'])
        }) {
            return Err("container environment cannot be encoded as an env file");
        }
        let directory = Builder::new()
            .prefix("dw-env-")
            .tempdir_in(&self.root)
            .map_err(|_| "private container environment directory unavailable")?;
        let file = directory.path().join("environment.list");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&file)
            .map_err(|_| "private container environment file unavailable")?;
        for (name, value) in environment {
            writeln!(output, "{name}={value}")
                .map_err(|_| "container environment write unavailable")?;
        }
        output
            .flush()
            .map_err(|_| "container environment flush unavailable")?;
        Ok(Box::new(NativeEnvironmentSession {
            _directory: directory,
            path: file.to_string_lossy().into_owned(),
        }))
    }
}

fn registry(image: &str, backend: Backend) -> Result<String, &'static str> {
    let first = image.split('/').next().ok_or("invalid registry image")?;
    let qualified =
        image.contains('/') && (first.contains('.') || first.contains(':') || first == "localhost");
    let registry = if qualified { first } else { "docker.io" };
    if registry.is_empty() || registry.contains(['@', '\\', '\0']) {
        return Err("invalid registry image");
    }
    Ok(if backend == Backend::Docker && registry == "docker.io" {
        "https://index.docker.io/v1/".into()
    } else {
        registry.into()
    })
}

fn docker_config_root() -> Option<PathBuf> {
    std::env::var_os("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".docker"))
        })
}

fn copy_private_contexts(source: &Path, target: &Path) -> Result<(), &'static str> {
    if !source.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(source).map_err(|_| "Docker context unavailable")?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("Docker context is not a directory");
    }
    fs::create_dir(target).map_err(|_| "private Docker context unavailable")?;
    for entry in fs::read_dir(source).map_err(|_| "Docker context unavailable")? {
        let entry = entry.map_err(|_| "Docker context unavailable")?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|_| "Docker context unavailable")?;
        if metadata.file_type().is_symlink() {
            return Err("Docker context contains a symlink");
        }
        let destination = target.join(entry.file_name());
        if metadata.is_dir() {
            copy_private_contexts(&entry.path(), &destination)?;
        } else if metadata.is_file() {
            let bytes = fs::read(entry.path()).map_err(|_| "Docker context unavailable")?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(destination)
                .and_then(|mut output| output.write_all(&bytes))
                .map_err(|_| "private Docker context unavailable")?;
        } else {
            return Err("Docker context contains an unsupported entry");
        }
    }
    Ok(())
}

fn docker_config_for_pull(
    source: Option<PathBuf>,
    target: &Path,
) -> Result<serde_json::Map<String, Value>, &'static str> {
    let mut config = serde_json::Map::new();
    let Some(source) = source else {
        return Ok(config);
    };
    let file = source.join("config.json");
    if file.exists() {
        config = serde_json::from_slice::<Value>(
            &fs::read(file).map_err(|_| "Docker config unavailable")?,
        )
        .map_err(|_| "Docker config invalid")?
        .as_object()
        .cloned()
        .ok_or("Docker config invalid")?;
        for key in ["auths", "credsStore", "credHelpers"] {
            config.remove(key);
        }
    }
    copy_private_contexts(&source.join("contexts"), &target.join("contexts"))?;
    Ok(config)
}

impl RegistryAuth for NativeRegistryAuth {
    fn prepare_anonymous(
        &self,
        backend: Backend,
    ) -> Result<Box<dyn RegistryAuthSession>, &'static str> {
        if backend == Backend::Apple {
            return Err("Apple registry auth is not qualified");
        }
        let directory = Builder::new()
            .prefix("dw-auth-")
            .tempdir_in(&self.root)
            .map_err(|_| "private registry auth directory unavailable")?;
        let file_name = if backend == Backend::Docker {
            "config.json"
        } else {
            "auth.json"
        };
        let file = directory.path().join(file_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&file)
            .map_err(|_| "registry auth file unavailable")?;
        let config = if backend == Backend::Docker {
            docker_config_for_pull(docker_config_root(), directory.path())?
        } else {
            serde_json::Map::new()
        };
        serde_json::to_writer(&mut output, &Value::Object(config))
            .map_err(|_| "registry auth encoding unavailable")?;
        output
            .flush()
            .map_err(|_| "registry auth flush unavailable")?;
        let environment = if backend == Backend::Docker {
            vec![(
                "DOCKER_CONFIG".into(),
                directory.path().to_string_lossy().into_owned(),
            )]
        } else {
            vec![
                (
                    "REGISTRY_AUTH_FILE".into(),
                    file.to_string_lossy().into_owned(),
                ),
                (
                    "HOME".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
            ]
        };
        Ok(Box::new(NativeAuthSession {
            _directory: directory,
            environment,
            redactions: Vec::new(),
        }))
    }

    fn prepare(
        &self,
        backend: Backend,
        image: &str,
        credential: &str,
    ) -> Result<Box<dyn RegistryAuthSession>, &'static str> {
        if backend == Backend::Apple {
            return Err("Apple registry auth is not qualified");
        }
        let credential: Credential =
            serde_json::from_str(credential).map_err(|_| "invalid registry credential")?;
        if credential.username.is_empty()
            || credential.username.len() > 1024
            || credential.username.contains([':', '\0', '\n', '\r'])
            || credential.password.is_empty()
            || credential.password.len() > 8192
            || credential.password.contains('\0')
        {
            return Err("invalid registry credential");
        }
        let registry = registry(image, backend)?;
        let token = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", credential.username, credential.password));
        let mut auths = serde_json::Map::new();
        auths.insert(registry, json!({"auth":token.clone()}));
        let directory = Builder::new()
            .prefix("dw-auth-")
            .tempdir_in(&self.root)
            .map_err(|_| "private registry auth directory unavailable")?;
        let file_name = if backend == Backend::Docker {
            "config.json"
        } else {
            "auth.json"
        };
        let file = directory.path().join(file_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options
            .open(&file)
            .map_err(|_| "registry auth file unavailable")?;
        let mut config = if backend == Backend::Docker {
            docker_config_for_pull(docker_config_root(), directory.path())?
        } else {
            serde_json::Map::new()
        };
        config.insert("auths".into(), Value::Object(auths));
        serde_json::to_writer(&mut output, &Value::Object(config))
            .map_err(|_| "registry auth encoding unavailable")?;
        output
            .flush()
            .map_err(|_| "registry auth flush unavailable")?;
        let environment = if backend == Backend::Docker {
            vec![(
                "DOCKER_CONFIG".into(),
                directory.path().to_string_lossy().into_owned(),
            )]
        } else {
            vec![
                (
                    "REGISTRY_AUTH_FILE".into(),
                    file.to_string_lossy().into_owned(),
                ),
                (
                    "HOME".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
            ]
        };
        Ok(Box::new(NativeAuthSession {
            _directory: directory,
            environment,
            redactions: vec![credential.password, token],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_registry_session_has_no_saved_credentials() {
        let sandbox = tempfile::tempdir().unwrap();
        let adapter = NativeRegistryAuth::new(sandbox.path().join("private")).unwrap();
        for backend in [Backend::Docker, Backend::Podman] {
            let session = adapter.prepare_anonymous(backend).unwrap();
            let environment = session.environment();
            let (name, value) = &environment[0];
            let file = if backend == Backend::Docker {
                assert_eq!(name, "DOCKER_CONFIG");
                PathBuf::from(value).join("config.json")
            } else {
                assert_eq!(name, "REGISTRY_AUTH_FILE");
                assert_eq!(environment[1].0, "HOME");
                assert_eq!(
                    PathBuf::from(&environment[1].1),
                    PathBuf::from(value).parent().unwrap()
                );
                PathBuf::from(value)
            };
            let config: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            assert!(config.get("auths").is_none());
            assert!(config.get("credsStore").is_none());
            assert!(config.get("credHelpers").is_none());
            drop(session);
            assert!(!file.exists());
        }
    }

    #[test]
    fn private_env_file_keeps_daemon_settings_out_of_cli_environment() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("private");
        let adapter = NativeRegistryAuth::new(root.clone()).unwrap();
        let environment = vec![("DOCKER_HOST".into(), "tcp://wrong-daemon:2375".into())];
        let session = adapter.prepare_workload(&environment).unwrap();
        let path = PathBuf::from(session.path());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "DOCKER_HOST=tcp://wrong-daemon:2375\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(session);
        assert!(!path.exists());
        let stale = root.join("dw-env-stale");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("environment.list"), "private stale data").unwrap();
        NativeRegistryAuth::new(root).unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn credentialed_pull_preserves_selected_docker_context_without_prior_credentials() {
        let sandbox = tempfile::tempdir().unwrap();
        let source = sandbox.path().join("docker-config");
        let target = sandbox.path().join("private");
        fs::create_dir_all(source.join("contexts/meta/hash")).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(
            source.join("config.json"),
            r#"{"currentContext":"remote","auths":{"old":{"auth":"old-secret"}},"credsStore":"desktop","credsStoreOther":"kept"}"#,
        )
        .unwrap();
        fs::write(
            source.join("contexts/meta/hash/meta.json"),
            r#"{"Name":"remote"}"#,
        )
        .unwrap();
        let config = docker_config_for_pull(Some(source), &target).unwrap();
        assert_eq!(config["currentContext"], "remote");
        assert_eq!(config["credsStoreOther"], "kept");
        assert!(!config.contains_key("auths"));
        assert!(!config.contains_key("credsStore"));
        assert_eq!(
            fs::read_to_string(target.join("contexts/meta/hash/meta.json")).unwrap(),
            r#"{"Name":"remote"}"#
        );
    }

    #[test]
    fn registry_secret_stays_in_private_one_pull_file_and_is_removed() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("registry-auth");
        let adapter = NativeRegistryAuth::new(root.clone()).unwrap();
        for backend in [Backend::Docker, Backend::Podman] {
            let session = adapter
                .prepare(
                    backend,
                    "registry.example.test:5000/team/image:tag",
                    r#"{"username":"agent","password":"secret-token"}"#,
                )
                .unwrap();
            let environment = session.environment();
            let (key, value) = &environment[0];
            assert!(!value.contains("secret-token"));
            let file = if backend == Backend::Docker {
                assert_eq!(key, "DOCKER_CONFIG");
                PathBuf::from(value).join("config.json")
            } else {
                assert_eq!(key, "REGISTRY_AUTH_FILE");
                PathBuf::from(value)
            };
            let contents = fs::read_to_string(&file).unwrap();
            assert!(!contents.contains("secret-token"));
            assert!(contents.contains("registry.example.test:5000"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            drop(session);
            assert!(!file.exists());
        }
        let stale = root.join("dw-auth-stale");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("auth.json"), "private stale data").unwrap();
        NativeRegistryAuth::new(root).unwrap();
        assert!(!stale.exists());
    }
}
