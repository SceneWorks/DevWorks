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
    use crate::protocol::Id;
    use crate::services::process::tools::RegisteredExecutable;

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
}
