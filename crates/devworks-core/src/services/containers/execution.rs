//! Bind an inert backend plan to the host's exact registered executable before dispatch.
use super::backend::Backend;
use crate::ports::process::ProcessSpec;
use crate::services::process::tools::{BindingRefusal, ToolBindings};
use std::path::PathBuf;

pub fn bound_spec(
    bindings: &ToolBindings,
    backend: Backend,
    args: Vec<String>,
    cwd: Option<PathBuf>,
) -> Result<ProcessSpec, BindingRefusal> {
    let program = bindings.resolve_named_executable(backend.executable())?;
    Ok(ProcessSpec {
        program,
        args,
        cwd,
        env: Vec::new(),
        clear_env: false,
        capture_output: true,
        no_child_processes: false,
        control_pipe: None,
        pipe_stdin: false,
        stdin: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::records::RegisteredPath;
    use crate::protocol::support::RelativePath;
    use crate::protocol::{Id, ProjectId};
    use crate::services::process::tools::{RegisteredExecutable, RegisteredWorkingRoot};

    #[test]
    fn selected_backend_requires_registered_exact_executable() {
        let path = std::env::current_exe().unwrap();
        let bindings = ToolBindings::new(
            [RegisteredExecutable {
                id: Id::new("docker").unwrap(),
                path: path.clone(),
            }],
            std::iter::empty(),
        )
        .unwrap();
        let spec = bound_spec(&bindings, Backend::Docker, vec!["--version".into()], None).unwrap();
        assert_eq!(spec.program, path.to_string_lossy());
        assert!(spec.capture_output);
        assert!(matches!(
            bound_spec(&bindings, Backend::Podman, Vec::new(), None),
            Err(BindingRefusal::UnknownExecutable)
        ));
    }

    #[test]
    fn registered_mount_path_is_confined_to_its_project_root() {
        let directory = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dw-container-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("file"), b"content").unwrap();
        let project = ProjectId(Id::new("project-a").unwrap());
        let bindings = ToolBindings::new(
            std::iter::empty::<RegisteredExecutable>(),
            [RegisteredWorkingRoot {
                id: Id::new("root-a").unwrap(),
                project: Some(project.clone()),
                path: directory.clone(),
            }],
        )
        .unwrap();
        let path = RegisteredPath {
            root_id: Id::new("root-a").unwrap(),
            relative: RelativePath("file".into()),
        };
        assert_eq!(
            bindings
                .resolve_registered_path(&path, Some(&project))
                .unwrap(),
            directory.join("file")
        );
        assert!(matches!(
            bindings.resolve_registered_path(&path, None),
            Err(BindingRefusal::OutOfProject)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(std::env::temp_dir(), directory.join("escape")).unwrap();
            let escaped = RegisteredPath {
                relative: RelativePath("escape".into()),
                ..path
            };
            assert!(matches!(
                bindings.resolve_registered_path(&escaped, Some(&project)),
                Err(BindingRefusal::OutOfProject)
            ));
        }
    }
}
