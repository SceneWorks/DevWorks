use crate::snapshot::Source;
use devworks_semantic_helper::{
    Failure,
    process::{limit_file_size, owned_group, run},
};
use protobuf::Message;
use scip::types::Index;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub(crate) const TOOLCHAIN: &str = "1.98.0";
pub(crate) const MAX_INDEX: u64 = 8 * 1024 * 1024;
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
    limit_file_size(&mut command, MAX_INDEX + 1);
    Ok(command)
}

#[derive(Deserialize)]
struct CargoWorkspace {
    workspace_root: PathBuf,
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    manifest_path: PathBuf,
}

fn project_metadata(
    cargo_check: &mut Command,
    output: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<CargoWorkspace, Failure> {
    run(
        cargo_check
            .args([
                "metadata",
                "--offline",
                "--no-deps",
                "--format-version",
                "1",
            ])
            .stdout(std::fs::File::create(output).map_err(|_| "scratch-write")?),
        deadline,
        cancelled,
        "offline-cargo-metadata-failed",
    )?;
    let mut bytes = Vec::new();
    std::fs::File::open(output)
        .map_err(|_| "cargo-metadata-missing")?
        .take(MAX_INDEX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cargo-metadata-unreadable")?;
    if bytes.len() as u64 > MAX_INDEX {
        return Err("cargo-metadata-budget".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid-cargo-metadata".into())
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
    let cargo_home = private.join("cargo");
    let metadata_path = private.join("cargo-metadata");
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
    let mut paths = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut workspace_manifests = BTreeSet::new();
    let mut result_bytes = 0u64;
    for project in projects {
        let candidate = repository.join(project);
        if workspace_manifests.contains(&candidate.join("Cargo.toml")) {
            continue;
        }
        let metadata = project_metadata(
            command(&cargo, &bin, private)?.current_dir(&candidate),
            &metadata_path,
            deadline,
            cancelled,
        )?;
        let root = metadata.workspace_root;
        let project = root
            .strip_prefix(&repository)
            .map_err(|_| "cargo-path-outside-source")?;
        for member in metadata.packages {
            if !member.manifest_path.starts_with(&repository) {
                return Err("cargo-path-outside-source".into());
            }
            workspace_manifests.insert(member.manifest_path);
        }
        if !roots.insert(root.clone()) {
            continue;
        }
        let mut cargo_check = command(&cargo, &bin, private)?;
        cargo_check
            .current_dir(&root)
            .args(["metadata", "--offline", "--format-version", "1"]);
        if source.copy_cached_dependencies(project, &original_home, &cargo_home)? {
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
        std::fs::remove_dir_all(&cargo_home).map_err(|_| "scratch-write")?;
        std::fs::create_dir(&cargo_home).map_err(|_| "scratch-write")?;
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
