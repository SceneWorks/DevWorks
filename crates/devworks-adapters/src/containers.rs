//! Private registry auth files scoped to one native container CLI pull.
use base64::Engine as _;
use devworks_core::services::containers::backend::Backend;
use devworks_core::services::containers::registry_auth::{RegistryAuth, RegistryAuthSession};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
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
            if !entry.file_name().to_string_lossy().starts_with("dw-auth-") {
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

impl RegistryAuth for NativeRegistryAuth {
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
        serde_json::to_writer(&mut output, &json!({"auths":Value::Object(auths)}))
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
            vec![(
                "REGISTRY_AUTH_FILE".into(),
                file.to_string_lossy().into_owned(),
            )]
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
