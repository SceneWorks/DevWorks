use crate::snapshot::Source;
use protobuf::Message;
use scip::types::Index;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) const TOOLCHAIN: &str = "1.98.0";
pub(crate) const MAX_INDEX: u64 = 8 * 1024 * 1024;
const MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SAMPLE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub stop_group: bool,
}

impl From<&'static str> for Failure {
    fn from(code: &'static str) -> Self {
        Self {
            code,
            stop_group: false,
        }
    }
}

impl Failure {
    fn stop(code: &'static str) -> Self {
        Self {
            code,
            stop_group: true,
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn owned_group() -> bool {
    // The DevWorks process owner puts the helper in a new group led by its own PID.
    unsafe { libc::getpgrp() == libc::getpid() }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn owned_group() -> bool {
    false
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn stop_group() -> ! {
    if owned_group() {
        // Only the process supervisor's group led by this helper, never the host's group.
        unsafe { libc::killpg(libc::getpid(), libc::SIGKILL) };
    }
    std::process::exit(1);
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn stop_group() -> ! {
    std::process::exit(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn memory() -> Result<u64, Failure> {
    if !owned_group() {
        return Err("unowned-process-group".into());
    }
    let mut members = [0 as libc::pid_t; 128];
    let bytes = std::mem::size_of_val(&members)
        .try_into()
        .expect("fixed buffer");
    // SAFETY: the process group is the helper's own, and members is a writable PID array.
    let count =
        unsafe { libc::proc_listpgrppids(libc::getpid(), members.as_mut_ptr().cast(), bytes) };
    let Ok(count) = usize::try_from(count) else {
        return Err(Failure::stop("process-observation-failed"));
    };
    if count == 0 || count >= members.len() {
        return Err(Failure::stop("compiler-process-budget"));
    }
    let mut total = 0u64;
    for pid in members[..count].iter().copied().filter(|id| *id > 0) {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: proc_pidinfo writes to a buffer sized for PROC_PIDTASKINFO.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n == size {
            // SAFETY: the size check confirms the entire value was written.
            total = total.saturating_add(unsafe { info.assume_init() }.pti_resident_size);
        }
    }
    Ok(total)
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn memory() -> Result<u64, Failure> {
    Err("unsupported-rust-pack-target".into())
}

fn budget(
    deadline: Instant,
    cancelled: &AtomicBool,
    observe: impl FnOnce() -> Result<u64, Failure>,
) -> Result<(), Failure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::stop("cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(Failure::stop("compiler-timeout"));
    }
    if observe()? > MEMORY_BYTES {
        return Err(Failure::stop("compiler-memory-budget"));
    }
    Ok(())
}

fn run(
    command: &mut Command,
    deadline: Instant,
    cancelled: &AtomicBool,
    failure: &'static str,
) -> Result<(), Failure> {
    budget(deadline, cancelled, memory)?;
    let mut child = command.stdin(Stdio::null()).spawn().map_err(|_| failure)?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err(Failure::stop(failure)),
            Ok(None) => {}
            Err(_) => return Err(Failure::stop(failure)),
        }
        budget(deadline, cancelled, memory)?;
        std::thread::sleep(SAMPLE);
    }
}

fn private_scratch() -> Result<tempfile::TempDir, Failure> {
    let parent = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| "scratch-unavailable")?;
    for root in parent.ancestors() {
        if [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ]
        .iter()
        .any(|config| root.join(config).symlink_metadata().is_ok())
        {
            return Err("unsafe-scratch-ancestor".into());
        }
    }
    tempfile::Builder::new()
        .prefix("devworks-rust-")
        .tempdir_in(parent)
        .map_err(|_| "scratch-unavailable".into())
}

fn toolchain(
    scratch: &Path,
    home: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<PathBuf, Failure> {
    let rustup = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|path| path.is_absolute())
        .map(|path| path.join("rustup"))
        .find(|path| path.is_file())
        .ok_or("rustup-missing")?;
    let output = scratch.join("toolchain-path");
    let stdout = std::fs::File::create(&output).map_err(|_| "scratch-write")?;
    run(
        Command::new(rustup)
            .args(["which", "--toolchain", TOOLCHAIN, "rust-analyzer"])
            .env_clear()
            .env("HOME", home)
            .current_dir(scratch)
            .stdout(stdout)
            .stderr(Stdio::null()),
        deadline,
        cancelled,
        "rust-analyzer-unavailable",
    )?;
    let reported = std::fs::read_to_string(output).map_err(|_| "rust-analyzer-unavailable")?;
    let analyzer = Path::new(reported.trim())
        .canonicalize()
        .map_err(|_| "rust-analyzer-unavailable")?;
    let bin = analyzer.parent().ok_or("rust-analyzer-unavailable")?;
    let sysroot = bin.parent().ok_or("rust-analyzer-unavailable")?;
    if analyzer.file_name().and_then(|name| name.to_str()) != Some("rust-analyzer")
        || !bin.join("cargo").is_file()
        || !bin.join("rustc").is_file()
        || !sysroot
            .join("lib/rustlib/src/rust/library/std/Cargo.toml")
            .is_file()
    {
        return Err("rust-toolchain-incomplete".into());
    }
    Ok(bin.to_path_buf())
}

fn command(program: &Path, bin: &Path, private: &Path) -> Result<Command, Failure> {
    let path = std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")])
        .map_err(|_| "unsafe-toolchain-path")?;
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs([
            ("CARGO", bin.join("cargo").into_os_string()),
            ("RUSTC", bin.join("rustc").into_os_string()),
            ("PATH", path),
            ("HOME", private.join("home").into_os_string()),
            ("XDG_CONFIG_HOME", private.join("home").into_os_string()),
            ("CARGO_HOME", private.join("cargo").into_os_string()),
            ("TMPDIR", private.join("temp").into_os_string()),
            ("CARGO_NET_OFFLINE", "true".into()),
            ("RUST_BACKTRACE", "0".into()),
        ])
        .stderr(Stdio::null());
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the only fork hook makes one async-signal-safe system call.
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: MAX_INDEX + 1,
                    rlim_max: MAX_INDEX + 1,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    Ok(command)
}

pub(crate) fn index(
    source: &Source,
    cancelled: &AtomicBool,
    deadline_ms: u32,
) -> Result<Vec<u8>, Failure> {
    if !owned_group() {
        return Err("unsupported-rust-pack-target".into());
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(deadline_ms).min(45_000));
    let scratch = private_scratch()?;
    let private = scratch.path();
    let repository = private.join("repository");
    for name in ["repository", "home", "cargo", "temp"] {
        std::fs::create_dir(private.join(name)).map_err(|_| "scratch-write")?;
    }
    let original_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("rustup-missing")?;
    let bin = toolchain(private, &original_home, deadline, cancelled)?;
    let analyzer = bin.join("rust-analyzer");
    let version_log = private.join("toolchain-version");
    run(
        command(&analyzer, &bin, private)?
            .arg("--version")
            .current_dir(private)
            .stdout(std::fs::File::create(&version_log).map_err(|_| "scratch-write")?),
        deadline,
        cancelled,
        "rust-analyzer-unavailable",
    )?;
    let version = std::fs::read_to_string(version_log).map_err(|_| "rust-analyzer-unavailable")?;
    if version.split_whitespace().nth(1) != Some(TOOLCHAIN) {
        return Err("unsupported-rust-analyzer-version".into());
    }

    let projects = source.materialize(&repository)?;
    source.copy_cached_dependencies(&original_home, &private.join("cargo"))?;
    let settings = private.join("analyzer.json");
    let cargo = bin.join("cargo");
    let config = serde_json::json!({
        "cargo": {
            "allTargets": false,
            "sysroot": bin.parent(),
            "extraEnv": {"CARGO_NET_OFFLINE": "true"},
            "buildScripts": {"overrideCommand": [
                cargo, "metadata", "--offline", "--no-deps", "--format-version", "1"
            ]}
        },
        "procMacro": {"enable": false}
    });
    std::fs::write(&settings, config.to_string()).map_err(|_| "scratch-write")?;
    let expected = source.rust_files();
    let mut merged = Index::default();
    let mut paths = std::collections::BTreeSet::new();
    let mut result_bytes = 0u64;
    for project in projects {
        let root = repository.join(&project);
        let mut cargo_check = command(&cargo, &bin, private)?;
        cargo_check
            .current_dir(&root)
            .args(["metadata", "--offline", "--format-version", "1"]);
        if root.join("Cargo.lock").is_file() {
            cargo_check.arg("--locked");
        }
        run(
            cargo_check.stdout(Stdio::null()),
            deadline,
            cancelled,
            "offline-cargo-metadata-failed",
        )?;
        let output = private.join("current.scip");
        run(
            command(&analyzer, &bin, private)?
                .current_dir(&root)
                .args(["scip", "."])
                .arg("--config-path")
                .arg(&settings)
                .arg("--output")
                .arg(&output)
                .args(["--exclude-vendored-libraries", "--num-threads", "1"])
                .stdout(Stdio::null()),
            deadline,
            cancelled,
            "rust-analyzer-failed",
        )?;
        let mut bytes = Vec::new();
        let remaining = MAX_INDEX.saturating_sub(result_bytes);
        std::fs::File::open(&output)
            .map_err(|_| "scip-output-missing")?
            .take(remaining + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "scip-output-unreadable")?;
        result_bytes += bytes.len() as u64;
        if result_bytes > MAX_INDEX {
            return Err("scip-output-budget".into());
        }
        std::fs::remove_file(output).map_err(|_| "scratch-write")?;
        let mut index = Index::parse_from_bytes(&bytes).map_err(|_| "invalid-scip")?;
        for document in &mut index.documents {
            let location = project.join(&document.relative_path);
            let relative = location.to_str().ok_or("invalid-scip-document")?;
            if !expected.contains_key(relative) || !paths.insert(relative.to_owned()) {
                return Err("scip-document-outside-capture".into());
            }
            document.relative_path = relative.into();
        }
        if merged.metadata.is_none() {
            if let Some(metadata) = index.metadata.as_mut() {
                // The scratch directory is not the source identity; the host binds the artifact.
                metadata.project_root.clear();
            }
            merged.metadata = index.metadata;
        }
        merged.documents.extend(index.documents);
        merged.external_symbols.extend(index.external_symbols);
    }
    if merged.documents.is_empty() {
        return Err("rust-analyzer-empty-index".into());
    }
    let result = merged.write_to_bytes().map_err(|_| "invalid-scip")?;
    if result.len() as u64 > MAX_INDEX {
        return Err("scip-output-budget".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_check_reports_memory_timeout_and_cancellation_as_faults() {
        let cancel = AtomicBool::new(false);
        let until = Instant::now() + Duration::from_secs(5);
        assert!(budget(until, &cancel, || Ok(MEMORY_BYTES)).is_ok());
        let error = budget(until, &cancel, || Ok(MEMORY_BYTES + 1)).unwrap_err();
        assert_eq!(error.code, "compiler-memory-budget");
        assert!(error.stop_group);
        let expired = budget(Instant::now(), &cancel, || Ok(0)).unwrap_err();
        assert_eq!(expired.code, "compiler-timeout");
        cancel.store(true, Ordering::Release);
        let cancelled = budget(until, &cancel, || Ok(0)).unwrap_err();
        assert_eq!(cancelled.code, "cancelled");
    }
}
