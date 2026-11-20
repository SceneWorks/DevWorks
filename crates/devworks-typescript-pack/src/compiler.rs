use crate::source::Source;
use devworks_core::ports::packages::RuntimeExecutable;
use devworks_semantic_helper::{
    Failure,
    process::{limit_file_size, owned_group, run},
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

fn asset_dir() -> Result<PathBuf, Failure> {
    let executable = std::env::current_exe().map_err(|_| "typescript-assets-unavailable")?;
    executable
        .parent()
        .and_then(Path::parent)
        .ok_or("typescript-assets-unavailable")?
        .join("extractor")
        .canonicalize()
        .map_err(|_| "typescript-assets-unavailable".into())
}

fn readable(path: &Path) -> Result<String, Failure> {
    let value = path.to_str().ok_or("typescript-unsupported-path")?;
    if value.contains(['*', '?']) {
        return Err("typescript-unsupported-path".into());
    }
    Ok(value.to_owned())
}

fn node(program: &Path, scratch: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .current_dir(scratch)
        .env("HOME", scratch)
        .env("TMPDIR", scratch)
        .env("PATH", "/usr/bin:/bin");
    limit_file_size(&mut command, MAX_OUTPUT + 1);
    command
}

fn restricted_node(
    program: &Path,
    assets: &Path,
    root: &Path,
    result: &Path,
) -> Result<Command, Failure> {
    let mut command = node(program, root);
    command
        .args([
            "--permission",
            "--no-addons",
            "--no-global-search-paths",
            "--max-old-space-size=1024",
        ])
        .arg(format!("--allow-fs-read={}", readable(assets)?))
        .arg(format!("--allow-fs-read={}", readable(root)?))
        .arg(format!("--allow-fs-write={}", readable(result)?));
    Ok(command)
}

fn bounded(path: &Path, max: u64) -> Result<Vec<u8>, Failure> {
    let file = std::fs::File::open(path).map_err(|_| "typescript-output-missing")?;
    let mut contents = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut contents)
        .map_err(|_| "typescript-output-unreadable")?;
    if contents.len() as u64 > max {
        return Err("typescript-output-budget".into());
    }
    Ok(contents)
}

fn current(executable: &RuntimeExecutable) -> bool {
    executable.program.is_absolute()
        && RuntimeExecutable::inspect(&executable.program)
            .as_ref()
            .ok()
            == Some(executable)
}

pub(crate) fn index(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline_ms: u32,
) -> Result<Vec<u8>, Failure> {
    if !owned_group() {
        return Err("unsupported-typescript-pack-target".into());
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(deadline_ms).min(45_000));
    let source = Source::parse(bytes)?;
    let runtime = &source.runtime;
    if !runtime.version.starts_with("24.") {
        return Err("unsupported-node-version".into());
    }
    if !current(&runtime.executable) {
        return Err("node-runtime-changed".into());
    }
    let assets = asset_dir()?;
    let base = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| "typescript-scratch-unavailable")?;
    let scratch = tempfile::Builder::new()
        .prefix("devworks-typescript-")
        .tempdir_in(base)
        .map_err(|_| "typescript-scratch-unavailable")?;
    let root = scratch.path();
    let version_path = root.join("node-version");
    let program = &runtime.executable.program;
    run(
        node(program, root)
            .arg("--version")
            .stdout(std::fs::File::create(&version_path).map_err(|_| "typescript-scratch-write")?)
            .stderr(Stdio::null()),
        deadline,
        cancelled,
        "node-version-probe-failed",
    )?;
    let version = bounded(&version_path, 256)?;
    let version = std::str::from_utf8(&version).map_err(|_| "unsupported-node-version")?;
    if version.trim().strip_prefix('v') != Some(runtime.version.as_str())
        || !current(&runtime.executable)
    {
        return Err("node-runtime-changed".into());
    }

    let tree = root.join("repository");
    std::fs::create_dir(&tree).map_err(|_| "typescript-scratch-write")?;
    let files = source.materialize(&tree)?;
    let request = root.join("request.json");
    let result = root.join("result.json");
    let diagnostics = root.join("diagnostics");
    std::fs::write(
        &request,
        serde_json::to_vec(&serde_json::json!({
            "repository": source.repository_id, "root": tree, "files": files,
        }))
        .map_err(|_| "invalid-typescript-source")?,
    )
    .map_err(|_| "typescript-scratch-write")?;
    if !current(&runtime.executable) {
        return Err("node-runtime-changed".into());
    }
    let compiled = run(
        restricted_node(program, &assets, root, &result)?
            .arg(assets.join("dist/devworks.js"))
            .args([&request, &result])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&diagnostics).map_err(|_| "typescript-scratch-write")?),
        deadline,
        cancelled,
        "typescript-compiler-failed",
    );
    if let Err(error) = compiled {
        if error.code == "typescript-compiler-failed"
            && let Ok(bytes) = bounded(&diagnostics, 128)
            && let Ok(code) = std::str::from_utf8(&bytes)
        {
            return Err(Failure {
                code: match code.trim() {
                    "invalid-typescript-job" => "invalid-typescript-job",
                    "typescript-analysis-incomplete" => "typescript-analysis-incomplete",
                    "invalid-definition-span" => "invalid-definition-span",
                    "definition-outside-capture" => "definition-outside-capture",
                    _ => error.code,
                },
                stop_group: error.stop_group,
            });
        }
        return Err(error);
    }
    if !current(&runtime.executable) {
        return Err("node-runtime-changed".into());
    }
    bounded(&result, MAX_OUTPUT)
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "executes the restricted native Node 24 runtime"]
    fn native_node_allows_only_the_captured_input_and_output_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let assets = root.join("assets");
        let scratch = root.join("scratch");
        std::fs::create_dir(&assets).unwrap();
        std::fs::create_dir(&scratch).unwrap();
        let input = scratch.join("input");
        let output = scratch.join("output");
        let sibling = root.join("sibling");
        std::fs::write(&input, "captured").unwrap();
        std::fs::write(&sibling, "uncaptured").unwrap();
        let program = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join("node"))
            .find(|path| path.is_file())
            .expect("Node 24");
        let script = assets.join("canary.cjs");
        std::fs::write(&script, r#"
const assert = require('node:assert/strict');
const fs = require('node:fs');
const v8 = require('node:v8');
const [input, output, outside] = process.argv.slice(2);
assert.equal(Number(process.versions.node.split('.')[0]), 24);
assert.equal(fs.readFileSync(input, 'utf8'), 'captured');
assert.throws(() => fs.readFileSync(outside, 'utf8'), { code: 'ERR_ACCESS_DENIED' });
assert.throws(() => fs.writeFileSync(input, 'tampered'), { code: 'ERR_ACCESS_DENIED' });
assert.throws(() => fs.writeFileSync(outside, 'tampered'), { code: 'ERR_ACCESS_DENIED' });
assert.throws(() => require('node:child_process').spawnSync(process.execPath, ['--version']), { code: 'ERR_ACCESS_DENIED' });
assert.throws(() => process.dlopen({}, './fake.node'), { code: 'ERR_DLOPEN_DISABLED' });
assert.ok(process.execArgv.includes('--max-old-space-size=1024'));
assert.ok(v8.getHeapStatistics().heap_size_limit < 1536 * 1024 * 1024);
assert.deepEqual(require('node:module').globalPaths, []);
fs.writeFileSync(output, 'qualified');
"#).unwrap();
        let capture = restricted_node(&program, &assets, &scratch, &output)
            .unwrap()
            .arg(&script)
            .args([&input, &output, &sibling])
            .output()
            .unwrap();
        assert!(
            capture.status.success(),
            "{}",
            String::from_utf8_lossy(&capture.stderr)
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "qualified");
        assert_eq!(std::fs::read_to_string(input).unwrap(), "captured");
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "uncaptured");
    }
}
