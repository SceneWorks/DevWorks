//! Bounded native availability probe for the exact backend selected by configuration.
use super::backend::Backend;
use super::execution::bound_spec;
use super::runner::{CliResult, ContainerRunner};
use crate::context::InvocationContext;
use crate::services::process::tools::ToolBindings;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostPlatform<'a> {
    pub os: &'a str,
    pub arch: &'a str,
    pub macos_major: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Unsupported,
    Unavailable,
    Healthy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub backend: Backend,
    pub availability: Availability,
    pub executable: Option<String>,
    pub version: Option<String>,
    pub detail: String,
}

fn successful(result: &CliResult) -> bool {
    result.exit_code == Some(0) && !result.truncated
}

pub fn probe(
    runner: &ContainerRunner,
    bindings: &ToolBindings,
    ctx: &InvocationContext,
    backend: Backend,
    host: HostPlatform<'_>,
) -> Probe {
    let mut outcome = Probe {
        backend,
        availability: Availability::Unavailable,
        executable: None,
        version: None,
        detail: String::new(),
    };
    if !backend.supported_host(host.os, host.arch, host.macos_major) {
        outcome.availability = Availability::Unsupported;
        outcome.detail = "unsupported host OS, CPU or system version".into();
        return outcome;
    }
    let command = |args| bound_spec(bindings, backend, args, None);
    let Ok(version_spec) = command(backend.version()) else {
        outcome.detail = "selected backend executable is not registered or has changed".into();
        return outcome;
    };
    outcome.executable = Some(version_spec.program.clone());
    let Ok(version) = runner.run(
        ctx,
        None,
        version_spec,
        Duration::from_secs(10),
        16_384,
        &[],
    ) else {
        outcome.detail = "backend version probe failed".into();
        return outcome;
    };
    if !successful(&version) {
        outcome.detail = "backend version probe exited unsuccessfully or was truncated".into();
        return outcome;
    }
    let version_text = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    if version_text.is_empty() {
        outcome.detail = "backend version probe returned no version".into();
        return outcome;
    }
    outcome.version = Some(version_text.chars().take(256).collect());
    let Ok(health_spec) = command(backend.health()) else {
        outcome.detail = "backend executable changed during health probe".into();
        return outcome;
    };
    let Ok(health) = runner.run(ctx, None, health_spec, Duration::from_secs(10), 65_536, &[])
    else {
        outcome.detail = "backend connection or service is unavailable".into();
        return outcome;
    };
    if !successful(&health) || serde_json::from_slice::<serde_json::Value>(&health.stdout).is_err()
    {
        outcome.detail = "backend health probe failed or returned invalid JSON".into();
        return outcome;
    }
    outcome.availability = Availability::Healthy;
    outcome.detail = "backend executable and connection are healthy".into();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_requires_exact_clean_exit_and_complete_output() {
        assert!(successful(&CliResult {
            exit_code: Some(0),
            stdout: b"{}".to_vec(),
            stderr: Vec::new(),
            truncated: false,
        }));
        assert!(!successful(&CliResult {
            exit_code: Some(0),
            stdout: b"{}".to_vec(),
            stderr: Vec::new(),
            truncated: true,
        }));
    }
}
