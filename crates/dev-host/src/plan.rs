//! `dev-host env`: where this worktree builds and keeps temp files, inside this host's folder
//! `<root>/hosts/<host>/` ([`Host`]).
//!
//! - `CARGO_TARGET_DIR` = `<host folder>/build/<project>/<build-key>/slot-<n>` (sticky per
//!   worktree; the slot registry sits beside the slots, so two hosts never share a slot even
//!   for the same worktree path).
//! - `TMPDIR` (and `TEMP`/`TMP` on Windows) = `<host folder>/t/<8-char id>`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::host::Host;
use crate::{Vars, config, digest, is_under, resolve, slots};

/// Environment variables that change build output but that cargo does not track, so builds
/// that differ in any of them must never share a target folder. An entry ending in `*` matches
/// every variable with that prefix.
///
/// - `MACOSX_DEPLOYMENT_TARGET`: `mlx-sys`'s build script has no `rerun-if-env-changed`, so a
///   shared folder would silently keep MLX built for the other target (dropping NAX kernels).
/// - `CARGO_PROFILE_*_STRIP` and `*_BUILD_OVERRIDE_STRIP`: set on Xcode 27 hosts so host
///   proc-macro dylibs keep their metadata; builds with and without them must not mix.
/// - `MLX_*`: MLX build flags read by its build script.
pub const HASHED_ENV_VARS: &[&str] = &[
    "MACOSX_DEPLOYMENT_TARGET",
    "CARGO_PROFILE_DEV_STRIP",
    "CARGO_PROFILE_TEST_STRIP",
    "CARGO_PROFILE_RELEASE_STRIP",
    "CARGO_PROFILE_BENCH_STRIP",
    "CARGO_PROFILE_DEV_BUILD_OVERRIDE_STRIP",
    "CARGO_PROFILE_TEST_BUILD_OVERRIDE_STRIP",
    "CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP",
    "CARGO_PROFILE_BENCH_BUILD_OVERRIDE_STRIP",
    "MLX_*",
];

/// Overrides for `env` and the shim. `None` means derive it.
#[derive(Debug, Default, Clone)]
pub struct Request {
    pub project: Option<String>,
    pub worktree: Option<PathBuf>,
    pub target: Option<String>,
    pub session: Option<String>,
    pub slots: Option<u32>,
    /// Mark this worktree's slot warm (reassigned and evicted last).
    pub warm: bool,
}

/// What a build key is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInputs {
    /// `rustc -vV` output, or the toolchain file's contents when rustc can't run.
    pub toolchain: String,
    pub target: String,
    /// The [`HASHED_ENV_VARS`] that are set, sorted by name.
    pub env: Vec<(String, String)>,
}

impl KeyInputs {
    pub fn new(toolchain: String, target: String, vars: &Vars) -> Self {
        let env = vars
            .iter()
            .filter(|(name, value)| !value.is_empty() && hashed(name))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        Self {
            toolchain,
            target,
            env,
        }
    }

    /// `<target>-<12 hex>`: readable target, hash over everything.
    pub fn key(&self) -> String {
        let mut parts = vec![self.toolchain.as_str(), self.target.as_str()];
        for (k, v) in &self.env {
            parts.push(k);
            parts.push(v);
        }
        format!("{}-{}", sanitize(&self.target), &digest(&parts)[..12])
    }

    fn describe(&self) -> String {
        let mut out = format!("target: {}\n", self.target);
        for (k, v) in &self.env {
            out.push_str(&format!("env: {k}={v}\n"));
        }
        out.push_str("toolchain:\n");
        out.push_str(&self.toolchain);
        out
    }
}

fn hashed(name: &str) -> bool {
    HASHED_ENV_VARS
        .iter()
        .any(|entry| match entry.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == *entry,
        })
}

/// Lower-case, keeping `[a-z0-9._-]` and mapping anything else to `-`.
pub fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '.' | '_' | '-') => c,
            _ => '-',
        })
        .collect()
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The repository name in a remote URL (`git@host:org/Name.git`, `https://host/org/Name`).
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let tail = url.trim_end_matches('/').rsplit(['/', ':', '\\']).next()?;
    let name = tail.strip_suffix(".git").unwrap_or(tail);
    (!name.is_empty()).then(|| name.to_owned())
}

/// The worktree's top folder, or `cwd` outside git.
pub fn worktree_of(cwd: &Path) -> PathBuf {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());
    resolve(&top, cwd)
}

/// The project name: the `origin` remote's repository name, else the main worktree's folder
/// name, else the worktree's folder name.
pub fn project_of(worktree: &Path) -> String {
    let from_remote = git(worktree, &["config", "--get", "remote.origin.url"])
        .and_then(|url| repo_name_from_url(&url));
    let from_main = || {
        let common = git(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = PathBuf::from(common);
        let dir = if common.file_name()? == ".git" {
            common.parent()?.to_path_buf()
        } else {
            common
        };
        let name = dir.file_name()?.to_string_lossy().into_owned();
        Some(name.strip_suffix(".git").unwrap_or(&name).to_owned())
    };
    let name = from_remote
        .or_else(from_main)
        .or_else(|| Some(worktree.file_name()?.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unnamed".to_owned());
    sanitize(&name)
}

/// The toolchain description and its host triple, from `rustc -vV` run in the worktree (so
/// `rust-toolchain.toml` and `RUSTUP_TOOLCHAIN` apply), else from the toolchain file.
fn toolchain(vars: &Vars, worktree: &Path) -> (String, Option<String>) {
    let rustc = vars.get("RUSTC").unwrap_or("rustc");
    if let Ok(out) = Command::new(rustc)
        .arg("-vV")
        .current_dir(worktree)
        .output()
        && out.status.success()
        && let Ok(text) = String::from_utf8(out.stdout)
    {
        let host = text
            .lines()
            .find_map(|l| l.strip_prefix("host: "))
            .map(str::to_owned);
        return (text, host);
    }
    for file in ["rust-toolchain.toml", "rust-toolchain"] {
        if let Ok(text) = std::fs::read_to_string(worktree.join(file)) {
            return (text, None);
        }
    }
    ("unknown toolchain".to_owned(), None)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The build folder for this worktree.
///
/// A caller-set `CARGO_TARGET_DIR` under the root is kept; one outside the root or inside
/// another host's folder is an error ([`Host::allow_build_dir`]).
pub fn target_dir(host: &Host, vars: &Vars, cwd: &Path, req: &Request) -> Result<PathBuf, String> {
    if let Some(set) = vars.get("CARGO_TARGET_DIR") {
        return host.allow_build_dir("CARGO_TARGET_DIR", Path::new(set), cwd);
    }
    let root = host.root();
    let worktree = match &req.worktree {
        Some(w) => resolve(w, cwd),
        None => worktree_of(cwd),
    };
    let project = req
        .project
        .as_deref()
        .map(sanitize)
        .unwrap_or_else(|| project_of(&worktree));
    let (toolchain, host_triple) = toolchain(vars, &worktree);
    let target = req
        .target
        .clone()
        .or_else(|| vars.get("CARGO_BUILD_TARGET").map(str::to_owned))
        .or(host_triple)
        .unwrap_or_else(|| format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS));
    let inputs = KeyInputs::new(toolchain, target, vars);
    let key_dir = host.dir().join("build").join(project).join(inputs.key());
    // A symlinked `hosts/`, host folder, `build/` (or level below it) would put the build off
    // the root's volume or into another host's folder.
    crate::confine(&key_dir, root, cwd)?;
    std::fs::create_dir_all(&key_dir)
        .map_err(|e| format!("cannot create {}: {e}", key_dir.display()))?;
    let key_file = key_dir.join("key.txt");
    if !key_file.exists() && !crate::is_link(&key_file) {
        // Informational only (for people and cleanup reports); a failed write doesn't matter.
        let _ = std::fs::write(&key_file, inputs.describe());
    }
    let cap = config::slots_per_key(req.slots, vars, &config::read(root)?.values)?;
    slots::assign(
        &key_dir,
        &worktree.to_string_lossy(),
        cap,
        now(),
        req.warm,
        &slots::CargoLockProbe,
    )
}

/// The 8-character temp id: from the session id when given, else from the GitHub Actions run
/// (run id, attempt, job, runner), else random.
pub fn tmp_id(vars: &Vars, session: Option<&str>) -> String {
    let session = session.or_else(|| vars.get("DEV_HOST_SESSION"));
    let seed = match (session, vars.get("GITHUB_RUN_ID")) {
        (Some(s), _) => format!("session:{s}"),
        (None, Some(run)) => format!(
            "run:{run}:{}:{}:{}",
            vars.get("GITHUB_RUN_ATTEMPT").unwrap_or("1"),
            vars.get("GITHUB_JOB").unwrap_or(""),
            vars.get("RUNNER_NAME").unwrap_or("")
        ),
        (None, None) => {
            use std::hash::{BuildHasher, RandomState};
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let noise = RandomState::new().hash_one((std::process::id(), nanos));
            format!("random:{noise}:{nanos}")
        }
    };
    digest(&[&seed])[..8].to_owned()
}

/// The temp folder variable this platform's tools read first.
const TMP_VAR: &str = if cfg!(windows) { "TEMP" } else { "TMPDIR" };

/// The temp folder (created, mode 0700 on Unix). An existing one already under this host's
/// `t/` is kept, so repeated calls in one session don't scatter folders.
pub fn tmp_dir(
    host: &Host,
    vars: &Vars,
    cwd: &Path,
    session: Option<&str>,
) -> Result<PathBuf, String> {
    let root = host.root();
    let t = host.dir().join("t");
    crate::confine(&t, root, cwd)?;
    if session.is_none()
        && let Some(current) = vars.get(TMP_VAR)
        && is_under(Path::new(current), &t, cwd)
        && resolve(Path::new(current), cwd) != resolve(&t, cwd)
    {
        let dir = resolve(Path::new(current), cwd);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        return Ok(dir);
    }
    let dir = t.join(tmp_id(vars, session));
    crate::confine(&dir, root, cwd)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    }
    Ok(dir)
}

/// Every assignment `env` prints, in order.
pub fn assignments(
    host: &Host,
    vars: &Vars,
    cwd: &Path,
    req: &Request,
) -> Result<Vec<(String, String)>, String> {
    let target = target_dir(host, vars, cwd, req)?;
    let tmp = tmp_dir(host, vars, cwd, req.session.as_deref())?;
    let tmp = tmp.to_string_lossy().into_owned();
    let mut out = vec![
        (
            "CARGO_TARGET_DIR".to_owned(),
            target.to_string_lossy().into_owned(),
        ),
        ("TMPDIR".to_owned(), tmp.clone()),
    ];
    if cfg!(windows) {
        out.push(("TEMP".to_owned(), tmp.clone()));
        out.push(("TMP".to_owned(), tmp));
    }
    Ok(out)
}

/// Output formats for `env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `export KEY='value'` for POSIX shells: `eval "$(dev-host env)"`.
    Sh,
    /// `KEY=value` lines for `$GITHUB_ENV`: `dev-host env --format github >> "$GITHUB_ENV"`.
    Github,
    /// `$env:KEY = 'value'` for PowerShell: `dev-host env --format powershell | Invoke-Expression`.
    Powershell,
}

impl Format {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "sh" => Some(Self::Sh),
            "github" => Some(Self::Github),
            "powershell" | "pwsh" => Some(Self::Powershell),
            _ => None,
        }
    }

    pub fn default_for_platform() -> Self {
        if cfg!(windows) {
            Self::Powershell
        } else {
            Self::Sh
        }
    }
}

pub fn render(assignments: &[(String, String)], format: Format) -> Result<String, String> {
    let mut out = String::new();
    for (key, value) in assignments {
        if value.contains(['\n', '\r']) {
            return Err(format!(
                "{key} contains a line break and cannot be exported"
            ));
        }
        let line = match format {
            Format::Sh => format!("export {key}='{}'", value.replace('\'', r"'\''")),
            Format::Github => format!("{key}={value}"),
            Format::Powershell => format!("$env:{key} = '{}'", value.replace('\'', "''")),
        };
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;

    fn key_with(vars: &[(&str, &str)]) -> String {
        let vars: Vars = vars.iter().copied().collect();
        KeyInputs::new("rustc 1.98.0".into(), "aarch64-apple-darwin".into(), &vars).key()
    }

    #[test]
    fn key_changes_with_deployment_target_strip_and_mlx_flags_only() {
        let base = key_with(&[]);
        assert_eq!(base, key_with(&[("PATH", "/x"), ("HOME", "/h")]));
        assert_ne!(base, key_with(&[("MACOSX_DEPLOYMENT_TARGET", "26.2")]));
        assert_ne!(
            key_with(&[("MACOSX_DEPLOYMENT_TARGET", "26.2")]),
            key_with(&[("MACOSX_DEPLOYMENT_TARGET", "15.0")])
        );
        assert_ne!(
            base,
            key_with(&[("CARGO_PROFILE_DEV_BUILD_OVERRIDE_STRIP", "none")])
        );
        assert_ne!(base, key_with(&[("MLX_BUILD_METAL", "ON")]));
        assert!(base.starts_with("aarch64-apple-darwin-"));
    }

    #[test]
    fn key_changes_with_toolchain_and_target() {
        let vars = Vars::default();
        let a = KeyInputs::new(
            "rustc 1.98.0".into(),
            "x86_64-pc-windows-msvc".into(),
            &vars,
        );
        let b = KeyInputs::new(
            "rustc 1.99.0".into(),
            "x86_64-pc-windows-msvc".into(),
            &vars,
        );
        let c = KeyInputs::new("rustc 1.98.0".into(), "aarch64-apple-darwin".into(), &vars);
        assert_ne!(a.key(), b.key());
        assert_ne!(a.key(), c.key());
    }

    #[test]
    fn repo_names_come_from_remote_urls() {
        assert_eq!(
            repo_name_from_url("git@github.com:Org/SceneWorks.git").as_deref(),
            Some("SceneWorks")
        );
        assert_eq!(
            repo_name_from_url("https://github.com/Org/inference/").as_deref(),
            Some("inference")
        );
        assert_eq!(sanitize("Scene Works"), "scene-works");
    }

    /// Host `name`'s folder (created) under the scratch root.
    fn host_named(scratch: &Scratch, name: &str) -> Host {
        let host = Host::new(scratch.path().join("root"), name).unwrap();
        std::fs::create_dir_all(host.dir()).unwrap();
        host
    }

    fn host(scratch: &Scratch) -> Host {
        host_named(scratch, "mac-a")
    }

    #[test]
    fn target_dir_is_sticky_per_worktree_and_separate_across_worktrees() {
        let scratch = Scratch::new("plan-sticky");
        let host = host(&scratch);
        let wt_a = scratch.path().join("a");
        let wt_b = scratch.path().join("b");
        std::fs::create_dir_all(&wt_a).unwrap();
        std::fs::create_dir_all(&wt_b).unwrap();
        let req = |w: &Path| Request {
            project: Some("demo".into()),
            worktree: Some(w.to_path_buf()),
            target: Some("aarch64-apple-darwin".into()),
            ..Request::default()
        };
        let vars = Vars::default();
        let a1 = target_dir(&host, &vars, scratch.path(), &req(&wt_a)).unwrap();
        let a2 = target_dir(&host, &vars, scratch.path(), &req(&wt_a)).unwrap();
        let b = target_dir(&host, &vars, scratch.path(), &req(&wt_b)).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.starts_with(host.dir().join("build").join("demo")));
        assert!(a1.join(slots::STAMP_FILE).is_file());

        let moved: Vars = [("MACOSX_DEPLOYMENT_TARGET", "15.0")].into_iter().collect();
        let a3 = target_dir(&host, &moved, scratch.path(), &req(&wt_a)).unwrap();
        assert_ne!(a1.parent(), a3.parent(), "a new key gets a new folder");
    }

    #[test]
    fn two_hosts_on_one_root_never_share_a_slot_or_temp_folder() {
        let scratch = Scratch::new("plan-hosts");
        let (mac_a, mac_b) = (host_named(&scratch, "mac-a"), host_named(&scratch, "mac-b"));
        let wt = scratch.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let req = Request {
            project: Some("demo".into()),
            worktree: Some(wt.clone()),
            target: Some("aarch64-apple-darwin".into()),
            session: Some("same-session".into()),
            ..Request::default()
        };
        let vars = Vars::default();
        let on = |host: &Host| assignments(host, &vars, scratch.path(), &req).unwrap();
        let (a, b) = (on(&mac_a), on(&mac_b));
        for (key, _) in &a {
            let value = |set: &[(String, String)]| {
                PathBuf::from(&set.iter().find(|(k, _)| k == key).unwrap().1)
            };
            let (in_a, in_b) = (value(&a), value(&b));
            assert!(in_a.starts_with(mac_a.dir()), "{key}: {}", in_a.display());
            assert!(in_b.starts_with(mac_b.dir()), "{key}: {}", in_b.display());
        }
        // Each host keeps its own registry, and nothing lands at the root's top level.
        let target = |set: &[(String, String)]| PathBuf::from(&set[0].1);
        for (host, set) in [(&mac_a, &a), (&mac_b, &b)] {
            let registry = target(set).parent().unwrap().join(slots::REGISTRY_FILE);
            assert!(registry.starts_with(host.dir().join("build")));
            assert!(registry.is_file());
        }
        let top: Vec<_> = std::fs::read_dir(mac_a.root())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(top, vec![std::ffi::OsString::from("hosts")]);
    }

    #[test]
    fn caller_target_dir_is_kept_under_root_and_refused_outside() {
        let scratch = Scratch::new("plan-caller");
        let host = host(&scratch);
        let inside = host.dir().join("build/custom");
        let vars: Vars = [("CARGO_TARGET_DIR", inside.display().to_string())]
            .into_iter()
            .collect();
        assert_eq!(
            target_dir(&host, &vars, scratch.path(), &Request::default()),
            Ok(resolve(&inside, scratch.path()))
        );
        let other = host.root().join("hosts/mac-b/build/custom");
        let vars: Vars = [("CARGO_TARGET_DIR", other.display().to_string())]
            .into_iter()
            .collect();
        let err = target_dir(&host, &vars, scratch.path(), &Request::default()).unwrap_err();
        assert!(err.contains("another host"), "{err}");
        let outside = scratch.path().join("target");
        let vars: Vars = [("CARGO_TARGET_DIR", outside.display().to_string())]
            .into_iter()
            .collect();
        let err = target_dir(&host, &vars, scratch.path(), &Request::default()).unwrap_err();
        assert!(err.contains("outside DEV_HOST_ROOT"), "{err}");
    }

    /// A host whose `level` (relative to the root) is a symlink to a folder outside the root.
    #[cfg(unix)]
    fn host_with_link(scratch: &Scratch, level: &str) -> (Host, PathBuf) {
        let host = Host::new(scratch.path().join("root"), "mac-a").unwrap();
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let link = host.root().join(level);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        (host, outside)
    }

    #[cfg(unix)]
    #[test]
    fn generated_target_dir_refuses_symlinked_levels_under_the_root() {
        for level in [
            "hosts",
            "hosts/mac-a",
            "hosts/mac-a/build",
            "hosts/mac-a/build/demo",
        ] {
            let scratch = Scratch::new("plan-link");
            let (host, outside) = host_with_link(&scratch, level);
            let req = Request {
                project: Some("demo".into()),
                worktree: Some(scratch.path().to_path_buf()),
                target: Some("aarch64-apple-darwin".into()),
                ..Request::default()
            };
            let err = target_dir(&host, &Vars::default(), scratch.path(), &req).unwrap_err();
            assert!(err.contains("is a symlink"), "{level}: {err}");
            assert!(
                err.contains(&host.root().join(level).display().to_string()),
                "{err}"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "{level}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn tmp_dir_refuses_a_symlinked_t() {
        let scratch = Scratch::new("plan-tlink");
        let (host, outside) = host_with_link(&scratch, "hosts/mac-a/t");
        let err = tmp_dir(&host, &Vars::default(), scratch.path(), Some("s")).unwrap_err();
        assert!(err.contains("is a symlink"), "{err}");
        let inherited: Vars = [(TMP_VAR, host.dir().join("t/abc").display().to_string())]
            .into_iter()
            .collect();
        assert!(tmp_dir(&host, &inherited, scratch.path(), None).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn tmp_dir_is_short_stable_per_session_and_created() {
        let scratch = Scratch::new("plan-tmp");
        let host = host(&scratch);
        let vars = Vars::default();
        let a = tmp_dir(&host, &vars, scratch.path(), Some("s-1")).unwrap();
        let b = tmp_dir(&host, &vars, scratch.path(), Some("s-1")).unwrap();
        let c = tmp_dir(&host, &vars, scratch.path(), Some("s-2")).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.is_dir());
        assert_eq!(a.file_name().unwrap().len(), 8);
        assert_eq!(a.parent().unwrap(), host.dir().join("t"));
        // A TMPDIR inherited from another host's folder is not kept.
        let foreign: Vars = [(
            TMP_VAR,
            host.root()
                .join("hosts/mac-b/t/abcd1234")
                .display()
                .to_string(),
        )]
        .into_iter()
        .collect();
        let d = tmp_dir(&host, &foreign, scratch.path(), None).unwrap();
        assert!(d.starts_with(host.dir().join("t")), "{}", d.display());

        let run: Vars = [("GITHUB_RUN_ID", "42"), ("GITHUB_RUN_ATTEMPT", "1")]
            .into_iter()
            .collect();
        assert_eq!(tmp_id(&run, None), tmp_id(&run, None));
        assert_ne!(
            tmp_id(&Vars::default(), None),
            tmp_id(&Vars::default(), None)
        );
    }

    #[test]
    fn render_formats_quote_safely() {
        let a = vec![("TMPDIR".to_owned(), "/r/t/it's".to_owned())];
        assert_eq!(
            render(&a, Format::Sh).unwrap(),
            "export TMPDIR='/r/t/it'\\''s'\n"
        );
        assert_eq!(render(&a, Format::Github).unwrap(), "TMPDIR=/r/t/it's\n");
        assert_eq!(
            render(&a, Format::Powershell).unwrap(),
            "$env:TMPDIR = '/r/t/it''s'\n"
        );
        let bad = vec![("X".to_owned(), "a\nb".to_owned())];
        assert!(render(&bad, Format::Github).is_err());
    }
}
