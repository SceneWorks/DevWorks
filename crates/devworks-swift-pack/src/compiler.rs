use crate::graph;
use crate::indexstore::{IndexStore, occurrence_budget};
use crate::source::Source;
use crate::workspace::{Module, Workspace};
use devworks_core::ports::indexing::Enrichment;
use devworks_semantic_helper::{
    Failure,
    process::{limit_file_size, owned_group, run_checked},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const LIMIT: u64 = 8 * 1024 * 1024;
const SCRATCH_LIMIT: u64 = 768 * 1024 * 1024;
const INVALID: &str = "swift-indexstore-invalid";
const WORKER_FAILED: &str = "swift-compiler-failed";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Compiled {
    succeeded: Vec<(usize, Module)>,
    gaps: BTreeMap<String, String>,
}

fn bounded(path: &Path, max: u64, fault: &'static str) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(max + 1).read_to_end(&mut bytes))
        .map_err(|_| fault)?;
    if bytes.len() as u64 > max {
        return Err("swift-output-budget".into());
    }
    Ok(bytes)
}

fn create(path: &Path) -> Result<(), Failure> {
    std::fs::create_dir_all(path).map_err(|_| "swift-scratch-write".into())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::write(path, bytes).map_err(|_| "swift-scratch-write".into())
}

fn scratch_size(root: &Path) -> Result<(), Failure> {
    let mut dirs = vec![root.to_path_buf()];
    let mut count = 0;
    let mut bytes = 0u64;
    while let Some(path) = dirs.pop() {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err("swift-scratch-unreadable".into()),
        };
        for entry in entries {
            let entry = entry.map_err(|_| "swift-scratch-unreadable")?;
            let info = match std::fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err("swift-scratch-unreadable".into()),
            };
            count += 1;
            bytes = bytes.saturating_add(info.len());
            if count > 65_536 || bytes > SCRATCH_LIMIT {
                return Err(Failure {
                    code: "swift-scratch-budget",
                    stop_group: true,
                });
            }
            if info.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    Ok(())
}

struct Work<'a> {
    root: &'a Path,
    deadline: Instant,
    cancelled: &'a AtomicBool,
}

impl Work<'_> {
    fn command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .current_dir(self.root)
            .env("HOME", self.root)
            .env("TMPDIR", self.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn run(&self, command: &mut Command, fault: &'static str) -> Result<ExitStatus, Failure> {
        let mut last = None;
        let status = run_checked(command, self.deadline, self.cancelled, fault, || {
            let now = Instant::now();
            if last.is_none_or(|then| now - then > Duration::from_secs(1)) {
                scratch_size(self.root)?;
                last = Some(now);
            }
            Ok(())
        })?;
        scratch_size(self.root)?;
        if status.code().is_none() {
            return Err(Failure {
                code: fault,
                stop_group: true,
            });
        }
        Ok(status)
    }

    fn compile(
        &self,
        source: &Source,
        workspace: &Workspace,
        id: usize,
        needed: bool,
        testable: bool,
    ) -> Result<bool, Failure> {
        let module = &workspace.modules[id];
        let dir = self.root.join("tasks").join(id.to_string());
        create(&dir)?;
        create(&dir.join("index"))?;
        let list = dir.join("files");
        let outputs = dir.join("outputs");
        let mut paths = String::new();
        let mut units = String::new();
        for (offset, path) in module.files.iter().enumerate() {
            paths.push_str(
                self.root
                    .join("source")
                    .join(path)
                    .to_str()
                    .ok_or("invalid-swift-source")?,
            );
            paths.push('\n');
            units.push_str(
                dir.join(format!("unit-{offset}.o"))
                    .to_str()
                    .ok_or("invalid-swift-source")?,
            );
            units.push('\n');
        }
        write(&list, paths.as_bytes())?;
        write(&outputs, units.as_bytes())?;
        let runtime = &source.runtime;
        let deployment = module.deployment.as_ref().map_or_else(
            || runtime.target.clone(),
            |version| format!("arm64-apple-macosx{version}"),
        );
        let make_command = || {
            let mut command = self.command(&runtime.frontend.program);
            command
                .args([
                    "-module-name",
                    &module.name,
                    "-target",
                    &deployment,
                    "-swift-version",
                    &module.swift_version,
                    "-D",
                    "DEBUG",
                ])
                .arg("-sdk")
                .arg(&runtime.sdk)
                .arg("-resource-dir")
                .arg(&runtime.resource_directory)
                .arg("-module-cache-path")
                .arg(self.root.join("cache"))
                .arg("-filelist")
                .arg(&list)
                .arg("-F")
                .arg(&runtime.frameworks)
                .arg("-I")
                .arg(&runtime.platform_libraries)
                .arg("-plugin-path")
                .arg(&runtime.plugins)
                .arg("-plugin-path")
                .arg(&runtime.testing_plugins)
                .arg("-in-process-plugin-server-path")
                .arg(&runtime.in_process_server)
                .arg("-external-plugin-path")
                .arg(format!(
                    "{}#{}",
                    runtime.platform_plugins.display(),
                    runtime.platform_server.display()
                ));
            if module.manifest.is_some() {
                command.args(["-D", "SWIFT_PACKAGE"]);
            }
            if let Some(name) = &module.package_name {
                command.args(["-package-name", name]);
            }
            if module.parse_as_library {
                command.arg("-parse-as-library");
            }
            if testable {
                command.arg("-enable-testing");
            }
            for define in &module.defines {
                command.args(["-D", define]);
            }
            for dep in workspace.closure(id) {
                command
                    .arg("-I")
                    .arg(self.root.join("tasks").join(dep.to_string()));
            }
            limit_file_size(&mut command, 128 * 1024 * 1024);
            command
        };
        if needed
            && !self
                .run(
                    make_command()
                        .arg("-emit-module")
                        .arg("-emit-module-path")
                        .arg(dir.join(format!("{}.swiftmodule", module.name))),
                    WORKER_FAILED,
                )?
                .success()
        {
            return Ok(false);
        }
        Ok(self
            .run(
                make_command()
                    .args([
                        "-typecheck",
                        "-num-threads",
                        "2",
                        "-index-ignore-clang-modules",
                        "-index-ignore-stdlib",
                    ])
                    .arg("-index-store-path")
                    .arg(dir.join("index"))
                    .arg("-output-filelist")
                    .arg(outputs),
                WORKER_FAILED,
            )?
            .success())
    }
}

fn runtime_current(source: &Source) -> Result<(), Failure> {
    if source.runtime.current() {
        Ok(())
    } else {
        Err("swift-runtime-changed".into())
    }
}

pub(crate) fn index(
    bytes: &[u8],
    cancelled: &AtomicBool,
    deadline_ms: u32,
) -> Result<Vec<u8>, Failure> {
    if !owned_group() {
        return Err("unsupported-swift-pack-target".into());
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(deadline_ms).min(45_000));
    let source = Source::parse(bytes)?;
    runtime_current(&source)?;
    let mut workspace = Workspace::build(&source.files)?;
    let order = workspace.order()?;
    if order.is_empty() {
        return Err("swift-project-unsupported".into());
    }
    let base = std::env::temp_dir()
        .canonicalize()
        .map_err(|_| "swift-scratch-unavailable")?;
    if base.to_str().is_none_or(|path| path.contains(['\n', '\r'])) {
        return Err("swift-scratch-unavailable".into());
    }
    let scratch = tempfile::Builder::new()
        .prefix("devworks-swift-")
        .tempdir_in(base)
        .map_err(|_| "swift-scratch-unavailable")?;
    let root = scratch.path();
    let staged = root.join("source");
    let request = root.join("source.json");
    let compiled = root.join("compiled.json");
    let output = root.join("output.json");
    write(&request, bytes)?;
    for file in source.files.iter().filter(|file| file.language == "swift") {
        let path = staged.join(&file.unit);
        create(path.parent().ok_or("invalid-swift-source")?)?;
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .and_then(|mut staged| staged.write_all(file.text.as_bytes()))
            .map_err(|_| "swift-scratch-write")?;
    }
    let work = Work {
        root,
        deadline,
        cancelled,
    };
    let needed: BTreeSet<_> = workspace
        .modules
        .iter()
        .flat_map(|module| module.dependencies.iter().copied())
        .collect();
    let testable: BTreeSet<_> = workspace
        .modules
        .iter()
        .enumerate()
        .filter(|(_, module)| module.testing)
        .flat_map(|(id, _)| workspace.closure(id))
        .collect();
    let mut succeeded = BTreeSet::new();
    for id in order {
        let module = &workspace.modules[id];
        let gap = if module.mixed {
            Some("swift-project-unsupported")
        } else if module.missing_dependency
            || module.files.is_empty()
            || module
                .dependencies
                .iter()
                .any(|dep| !succeeded.contains(dep))
        {
            Some("swift-dependency-unavailable")
        } else if work.compile(
            &source,
            &workspace,
            id,
            needed.contains(&id),
            testable.contains(&id),
        )? {
            succeeded.insert(id);
            None
        } else {
            Some(WORKER_FAILED)
        };
        if let Some(gap) = gap {
            for file in &module.files {
                workspace.gaps.insert(file.clone(), gap.into());
            }
        }
    }
    if succeeded.is_empty() {
        return Err(if workspace.gaps.values().any(|gap| gap == WORKER_FAILED) {
            WORKER_FAILED
        } else if workspace
            .gaps
            .values()
            .any(|gap| gap == "swift-dependency-unavailable")
        {
            "swift-dependency-unavailable"
        } else {
            "swift-project-unsupported"
        }
        .into());
    }
    runtime_current(&source)?;
    let decode_plan = Compiled {
        succeeded: workspace
            .modules
            .into_iter()
            .enumerate()
            .filter(|(id, _)| succeeded.contains(id))
            .collect(),
        gaps: workspace.gaps,
    };
    write(
        &compiled,
        &serde_json::to_vec(&decode_plan).map_err(|_| INVALID)?,
    )?;
    let executable = std::env::current_exe().map_err(|_| INVALID)?;
    let mut decoder = work.command(&executable);
    decoder.arg("--decode-index").arg(root);
    limit_file_size(&mut decoder, LIMIT + 1);
    if !work.run(&mut decoder, INVALID)?.success() {
        return Err(
            match bounded(&root.join("fault"), 64, INVALID).ok().as_deref() {
                Some(b"swift-output-budget") => "swift-output-budget",
                Some(b"swift-runtime-changed") => "swift-runtime-changed",
                _ => INVALID,
            }
            .into(),
        );
    }
    runtime_current(&source)?;
    let bytes = bounded(&output, LIMIT, INVALID)?;
    let result = devworks_core::framing::parse(&bytes).map_err(|_| INVALID)?;
    let units = result
        .as_object()
        .filter(|map| map.len() == 1)
        .and_then(|map| map.get("units"))
        .ok_or(INVALID)?;
    serde_json::from_value::<Vec<Enrichment>>(units.clone()).map_err(|_| INVALID)?;
    Ok(bytes)
}

struct Output {
    file: std::fs::File,
    remaining: u64,
    exceeded: bool,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            self.exceeded = true;
            return Err(std::io::Error::other("Swift index output budget exceeded"));
        }
        let length = self.file.write(bytes)?;
        self.remaining -= length as u64;
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn decode_result(root: &Path) -> Result<(), Failure> {
    let bytes = bounded(&root.join("source.json"), LIMIT, INVALID)?;
    let source = Source::parse(&bytes)?;
    runtime_current(&source)?;
    let plan = bounded(&root.join("compiled.json"), LIMIT, INVALID)?;
    let plan: Compiled = serde_json::from_slice(&plan).map_err(|_| INVALID)?;
    if plan.succeeded.is_empty() || plan.succeeded.len() > 64 {
        return Err(INVALID.into());
    }
    let index_store = IndexStore::open(&source.runtime.index_store.program)?;
    let mut groups = Vec::new();
    let mut remaining = occurrence_budget();
    for (id, module) in plan.succeeded {
        let paths: BTreeMap<PathBuf, String> = module
            .files
            .iter()
            .filter(|path| devworks_core::ports::packages::swift::source_path(path))
            .map(|path| (root.join("source").join(path), path.clone()))
            .collect();
        if paths.len() != module.files.len() {
            return Err(INVALID.into());
        }
        let records = index_store.read(
            &root.join("tasks").join(id.to_string()).join("index"),
            &module.name,
            &paths,
            &mut remaining,
        )?;
        groups.push((module, records));
    }
    let units = graph::project(&source.repository_name, &source.files, &groups, &plan.gaps)?;
    let file = std::fs::File::create(root.join("output.json")).map_err(|_| INVALID)?;
    let mut output = Output {
        file,
        remaining: LIMIT,
        exceeded: false,
    };
    #[derive(Serialize)]
    struct ResultGraph {
        units: Vec<Enrichment>,
    }
    if serde_json::to_writer(&mut output, &ResultGraph { units }).is_err() {
        return Err(if output.exceeded {
            "swift-output-budget"
        } else {
            INVALID
        }
        .into());
    }
    Ok(())
}

pub(crate) fn decode() -> Result<(), Failure> {
    let mut args = std::env::args_os().skip(2);
    let root = PathBuf::from(args.next().ok_or(INVALID)?);
    if !root.is_absolute()
        || args.next().is_some()
        || std::env::current_dir().ok().as_ref() != Some(&root)
    {
        return Err(INVALID.into());
    }
    let result = decode_result(&root);
    if let Err(error) = &result {
        let _ = write(&root.join("fault"), error.code.as_bytes());
    }
    result
}
