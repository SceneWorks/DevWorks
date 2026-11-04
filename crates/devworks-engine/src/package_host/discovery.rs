//! What the package host discovers outside the registry: the runtimes declared prerequisites
//! need (probed by running each runtime's version command as an owned process with a short
//! timeout, never on a UI or write thread) and the packs signed publisher indexes offer
//! (fetched through the index transport port and verified against each source's key).
use super::{PackageHost, child_environment};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::packages::{
    IndexTransport, IndexVerifier, MAX_INDEX_BYTES, RuntimeCache, RuntimeObservation,
};
use devworks_core::ports::process::ProcessSpec;
use devworks_core::ports::storage::{ReadOptions, read_snapshot};
use devworks_core::services::packages::{index, policy};
use devworks_core::services::process::{LaunchPolicy, LaunchRequest};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runtimes probed whether or not a package declares them yet.
pub const KNOWN_RUNTIMES: [&str; 5] = ["dotnet", "node", "swift", "cargo", "rustc"];

/// The index ports.
pub struct IndexPorts {
    pub transport: Arc<dyn IndexTransport>,
    pub verifier: Arc<dyn IndexVerifier>,
}

/// The runtime ports: the cache availability reads and, for tests and portable installs, the
/// `PATH` the version commands are resolved on (`None`: the host's own).
pub struct RuntimePorts {
    pub cache: Arc<RuntimeCache>,
    pub search_path: Option<String>,
}

/// A declared runtime ID that names a command (the generic form: `<id> --version`).
fn command_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        && !id.starts_with(['.', '-'])
}

/// Resolves a runtime command on `path` (a `PATH`-style list) the way a shell would: the
/// first directory holding an executable file named `name`, or on Windows `name` plus one of
/// the `pathext` extensions (`PATHEXT`, default `.COM;.EXE;.BAT;.CMD`), so `node.cmd` and
/// `dotnet.exe` shims are found. `None` when nothing matches.
pub fn resolve_command(name: &str, path: &str, pathext: Option<&str>) -> Option<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        pathext
            .unwrap_or(".COM;.EXE;.BAT;.CMD")
            .split(';')
            .map(str::trim)
            .filter(|e| e.starts_with('.') && e.len() > 1)
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            // A relative PATH entry would resolve against the host's working directory.
            continue;
        }
        let mut candidates = vec![dir.join(name)];
        candidates.extend(extensions.iter().map(|e| dir.join(format!("{name}{e}"))));
        // On Windows a bare name without an extension is not runnable.
        let start = usize::from(cfg!(windows));
        for c in candidates.into_iter().skip(start) {
            if is_executable(&c) {
                return Some(c);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// The version a version command printed: the first `[v]N(.N)*` token, normalized to
/// `major.minor.patch` (`v20.11.1`, `8.0.204`, `cargo 1.82.0 (…)`, `Apple Swift version 6.0.3`).
pub fn parse_version(output: &str) -> Option<String> {
    for token in output.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
        let t = token.trim_start_matches('v').trim_start_matches("swift-");
        let parts: Vec<&str> = t.split(['.', '-']).take(3).collect();
        if parts.is_empty() || !parts[0].chars().all(|c| c.is_ascii_digit()) || parts[0].is_empty()
        {
            continue;
        }
        let nums: Vec<u64> = parts.iter().map_while(|p| p.parse::<u64>().ok()).collect();
        if nums.len() >= 2 || (nums.len() == 1 && t.contains('.')) {
            let n = |i: usize| nums.get(i).copied().unwrap_or(0);
            return Some(format!("{}.{}.{}", n(0), n(1), n(2)));
        }
    }
    None
}

impl PackageHost {
    /// Every runtime to probe: the known ones and every runtime a recorded version declares.
    fn declared_runtimes(&self) -> BTreeSet<String> {
        let svc = self.service.clone();
        let mut out: BTreeSet<String> = KNOWN_RUNTIMES.iter().map(|s| (*s).to_owned()).collect();
        let declared = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            let read = s.packages();
            let mut ids = Vec::new();
            for (id, (p, _)) in svc.registry(read)? {
                for d in [&p.active_digest, &p.pending_digest].into_iter().flatten() {
                    if let Some(i) = read
                        .package_version(&id, d)?
                        .as_ref()
                        .and_then(policy::inspection)
                    {
                        ids.extend(i.runtimes.into_iter().map(|r| r.runtime_id));
                    }
                }
            }
            Ok(ids)
        })
        .unwrap_or_default();
        out.extend(declared.into_iter().filter(|r| command_name(r)));
        out
    }

    /// Probes one runtime: `<runtime> --version` as an owned process, bounded by
    /// [`super::HostConfig::runtime_timeout`].
    fn probe(&self, runtime: &str, search_path: Option<&str>) -> RuntimeObservation {
        let mut env = child_environment();
        let var = |env: &[(String, String)], k: &str| {
            env.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        if let Some(path) = search_path {
            env.retain(|(k, _)| !k.eq_ignore_ascii_case("PATH"));
            env.push(("PATH".into(), path.to_owned()));
        }
        // Resolved here (PATH, and PATHEXT on Windows): the launch gets the full path, so a
        // `.cmd` shim runs through its interpreter and nothing depends on the spawner's lookup.
        let path = var(&env, "PATH").unwrap_or_default();
        let Some(program) = resolve_command(runtime, &path, var(&env, "PATHEXT").as_deref()) else {
            return RuntimeObservation::Missing(format!("`{runtime}` was not found on PATH"));
        };
        let mut spec = ProcessSpec::new(&[
            program.to_string_lossy().into_owned(),
            "--version".to_owned(),
        ])
        .expect("argv");
        spec.clear_env = true;
        spec.env = env;
        spec.capture_output = true;
        let ctx = self.host_ctx();
        let policy = LaunchPolicy {
            fs: &*self.launch.fs,
            roots: &self.launch.roots,
            protected_branches: &self.launch.protected_branches,
        };
        let owned = match self.supervisor.launch(
            &ctx,
            LaunchRequest {
                spec,
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        ) {
            Ok(o) => o,
            Err(_) => return RuntimeObservation::Missing("not found".into()),
        };
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        if let Some((stdout, stderr)) = self.supervisor.take_output(&owned) {
            for mut pipe in [stdout, stderr] {
                let sink = out.clone();
                readers.push(std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = pipe.by_ref().take(64 * 1024).read_to_end(&mut buf);
                    sink.lock().unwrap_or_else(|p| p.into_inner()).extend(buf);
                }));
            }
        }
        let timeout = self.config.runtime_timeout;
        let finished = self.supervisor.wait(&owned, timeout).ok().flatten();
        if finished.is_none() {
            let _ =
                self.supervisor
                    .cancel(&owned, Duration::from_millis(50), Duration::from_secs(2));
        }
        for r in readers {
            let _ = r.join();
        }
        if finished.is_none() {
            return RuntimeObservation::Missing(format!(
                "`{runtime} --version` did not answer within {} ms",
                timeout.as_millis()
            ));
        }
        let text =
            String::from_utf8_lossy(&out.lock().unwrap_or_else(|p| p.into_inner())).into_owned();
        match finished {
            Some(devworks_core::services::process::StopOutcome::Reaped {
                exit_code: Some(0),
                ..
            }) => match parse_version(&text) {
                Some(v) => RuntimeObservation::Found(v),
                None => {
                    RuntimeObservation::Missing(format!("`{runtime} --version` printed no version"))
                }
            },
            _ => RuntimeObservation::Missing(format!("`{runtime} --version` failed")),
        }
    }

    /// Probes every known and declared runtime, refreshes the cache and announces any
    /// availability change. Blocking; runs on the host's own thread (or a caller's worker).
    pub fn refresh_runtimes(&self) -> BTreeMap<String, RuntimeObservation> {
        let Some(ports) = &self.runtimes else {
            return BTreeMap::new();
        };
        let mut seen = BTreeMap::new();
        for r in self.declared_runtimes() {
            let o = self.probe(&r, ports.search_path.as_deref());
            seen.insert(r, o);
        }
        ports.cache.set(seen.clone());
        let _ = self.service.observe_now("RuntimeProbe");
        seen
    }

    /// Fetches and verifies every configured index source, records each outcome and announces
    /// any availability change. Blocking (network); runs on the host's own thread.
    pub fn refresh_indexes(&self) -> usize {
        let Some(ports) = &self.index else {
            return 0;
        };
        let Ok(sources) = self.service.configured_indexes() else {
            return 0;
        };
        let _ = self.service.retain_configured_indexes(&sources);
        let mut verified = 0;
        for source in &sources {
            let outcome = ports
                .verifier
                .index_location(source)
                .and_then(|location| ports.transport.fetch(&location, MAX_INDEX_BYTES))
                .and_then(|bytes| ports.verifier.verify(source, &bytes));
            verified += usize::from(outcome.is_ok());
            let _ = self.service.record_index(&sources, source, outcome);
        }
        verified
    }

    /// Downloads exactly the bundle a verified index offers for `(package, manifest digest)`
    /// into `dest`, accepting it only when its bytes hash to the signed bundle digest and
    /// its manifest to the signed manifest digest. The caller then installs it through the
    /// ordinary inspector and approval path.
    pub fn fetch_bundle(
        &self,
        package_id: &str,
        manifest_digest: &str,
        dest: &Path,
    ) -> Result<PathBuf, String> {
        let ports = self
            .index
            .as_ref()
            .ok_or("no package index is configured")?;
        let (pkg, digest) = (package_id.to_owned(), manifest_digest.to_owned());
        let offer = read_snapshot(&*self.store, &ReadOptions::default(), move |s| {
            index::offer(s.packages(), &pkg, &digest)
        })
        .map_err(|e| format!("{e:?}"))?
        .ok_or("no verified index offers this version")?;
        let bytes = ports
            .transport
            .fetch(&offer.entry.bundle, offer.entry.bundle_bytes)?;
        if bytes.len() as u64 != offer.entry.bundle_bytes
            || sha256_hex(&bytes) != offer.entry.bundle_digest
        {
            return Err("the downloaded bundle does not match the signed index".into());
        }
        let inspection = self
            .service
            .inspect(&bytes)
            .map_err(|e| format!("{}: {}", e.code(), e.detail()))?;
        if inspection.package_id != offer.entry.package_id
            || inspection.manifest_digest != offer.entry.manifest_digest
            || inspection.version != offer.entry.version
        {
            return Err("the downloaded bundle is not the version the signed index lists".into());
        }
        std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
        let path = dest.join(format!(
            "{}-{}.dwpkg",
            inspection.package_id, inspection.version
        ));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }
}
