use crate::source::Source;
use devworks_core::ports::indexing::Enrichment;
use devworks_semantic_helper::{
    Failure,
    process::{limit_file_size, owned_group, run},
};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

fn bounded(path: &Path, max: u64) -> Result<Vec<u8>, Failure> {
    let mut contents = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(max + 1).read_to_end(&mut contents))
        .map_err(|_| "invalid-dotnet-index")?;
    if contents.len() as u64 > max {
        return Err("dotnet-output-budget".into());
    }
    Ok(contents)
}

pub(crate) fn index(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline_ms: u32,
) -> Result<Vec<u8>, Failure> {
    if !owned_group() {
        return Err("unsupported-dotnet-pack-target".into());
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(deadline_ms).min(45_000));
    let source = Source::parse(bytes)?;
    if !source.runtime.current() {
        return Err("dotnet-runtime-changed".into());
    }

    let executable = std::env::current_exe().map_err(|_| "dotnet-assets-unavailable")?;
    let installed = executable
        .parent()
        .and_then(Path::parent)
        .ok_or("dotnet-assets-unavailable")?;
    let worker = installed.join("compiler/DevWorks.Dotnet.dll");
    if !worker.is_file() {
        return Err("dotnet-assets-unavailable".into());
    }
    let base = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| "dotnet-scratch-unavailable")?;
    let scratch = tempfile::Builder::new()
        .prefix("devworks-dotnet-")
        .tempdir_in(base)
        .map_err(|_| "dotnet-scratch-unavailable")?;
    let root = scratch.path();
    let request = root.join("source.json");
    let output = root.join("index.json");
    let fault = root.join("fault");
    std::fs::write(&request, bytes).map_err(|_| "dotnet-scratch-write")?;
    let mut command = Command::new(&source.runtime.executable.program);
    command
        .env_clear()
        .current_dir(root)
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("DOTNET_CLI_HOME", root)
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_GCHeapHardLimit", "0x40000000")
        .args(["exec", "--fx-version", &source.runtime.runtime_version])
        .arg(worker)
        .args([&request, &output, &fault])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    limit_file_size(&mut command, MAX_OUTPUT + 1);
    run(&mut command, deadline, cancelled, "dotnet-worker-failed")?;
    if !source.runtime.current() {
        return Err("dotnet-runtime-changed".into());
    }
    if fault.exists() {
        return Err(match bounded(&fault, 64)?.as_slice() {
            b"dotnet-project-unsupported" => "dotnet-project-unsupported",
            b"dotnet-runtime-changed" => "dotnet-runtime-changed",
            b"dotnet-output-budget" => "dotnet-output-budget",
            b"compiler-memory-budget" => "compiler-memory-budget",
            _ => "invalid-dotnet-index",
        }
        .into());
    }
    let result = bounded(&output, MAX_OUTPUT)?;
    let document = devworks_core::framing::parse(&result).map_err(|_| "invalid-dotnet-index")?;
    let units = document
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("units"))
        .ok_or("invalid-dotnet-index")?;
    serde_json::from_value::<Vec<Enrichment>>(units.clone()).map_err(|_| "invalid-dotnet-index")?;
    Ok(result)
}
