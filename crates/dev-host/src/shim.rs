//! The `cargo` shim: a backstop for builds whose environment didn't run `dev-host env`.
//!
//! Installed as `cargo` (a link or copy of `dev-host` placed ahead of the real cargo on
//! `PATH`), or run as `dev-host cargo -- <args>`. It refuses when `check` fails or when
//! `CARGO_TARGET_DIR` or a `--target-dir` argument points outside the root, fills `CARGO_TARGET_DIR` in when unset, and then
//! runs the real cargo: `DEV_HOST_REAL_CARGO` if set, else the first `cargo` on `PATH` that is
//! not this program, else `rustup which cargo`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::check::VolumeProbe;
use crate::{Vars, plan};

pub const REAL_CARGO_VAR: &str = "DEV_HOST_REAL_CARGO";
/// Counts nested shim launches; past [`MAX_DEPTH`] the shim assumes it is looping on itself.
pub const DEPTH_VAR: &str = "DEV_HOST_SHIM_DEPTH";
pub const MAX_DEPTH: u32 = 8;

const CARGO_EXE: &str = if cfg!(windows) { "cargo.exe" } else { "cargo" };

/// What the shim will run.
#[derive(Debug, PartialEq, Eq)]
pub struct Launch {
    pub cargo: PathBuf,
    pub target_dir: PathBuf,
    pub depth: u32,
}

/// Decides the launch for cargo `args`, or why the shim refuses.
pub fn prepare(
    vars: &Vars,
    cwd: &Path,
    self_exe: &Path,
    args: &[OsString],
    check: impl FnOnce(&Vars) -> Result<PathBuf, String>,
) -> Result<Launch, String> {
    let depth = vars
        .get(DEPTH_VAR)
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    if depth > MAX_DEPTH {
        return Err(format!(
            "nested {depth} levels deep; the shim is probably finding itself as cargo. \
             Set {REAL_CARGO_VAR} to the real cargo"
        ));
    }
    let root = check(vars).map_err(|e| format!("dev-host check failed: {e}"))?;
    check_target_dir_args(args, &root, cwd)?;
    let target_dir = plan::target_dir(&root, vars, cwd, &plan::Request::default())?;
    let cargo = find_real_cargo(vars, self_exe).ok_or_else(|| {
        format!("cannot find the real cargo on PATH or through rustup; set {REAL_CARGO_VAR}")
    })?;
    Ok(Launch {
        cargo,
        target_dir,
        depth,
    })
}

const TARGET_DIR_FLAG: &str = "--target-dir";

/// Refuses a `--target-dir PATH` or `--target-dir=PATH` cargo argument outside `root`: the flag
/// overrides `CARGO_TARGET_DIR`. Scanning stops at a bare `--`, after which arguments belong to
/// the program cargo runs, not to cargo.
fn check_target_dir_args(args: &[OsString], root: &Path, cwd: &Path) -> Result<(), String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let value: Option<&OsStr> = if arg == TARGET_DIR_FLAG {
            // Without a value cargo reports the error itself.
            iter.next().map(OsString::as_os_str)
        } else if arg
            .as_encoded_bytes()
            .starts_with(format!("{TARGET_DIR_FLAG}=").as_bytes())
        {
            let Some(text) = arg.to_str() else {
                return Err(format!(
                    "{} is not valid UTF-8; dev-host cannot check it",
                    arg.to_string_lossy()
                ));
            };
            Some(OsStr::new(&text[TARGET_DIR_FLAG.len() + 1..]))
        } else {
            None
        };
        if let Some(value) = value {
            let path = Path::new(value);
            if !crate::is_under(path, root, cwd) {
                return Err(format!(
                    "{TARGET_DIR_FLAG} {} is outside DEV_HOST_ROOT={}; drop it so dev-host \
                     assigns a slot, or point it under the root",
                    path.display(),
                    root.display()
                ));
            }
        }
    }
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The real cargo, never `self_exe` (compared after resolving links).
pub fn find_real_cargo(vars: &Vars, self_exe: &Path) -> Option<PathBuf> {
    if let Some(explicit) = vars.get(REAL_CARGO_VAR) {
        let explicit = PathBuf::from(explicit);
        return (!same_file(&explicit, self_exe)).then_some(explicit);
    }
    let path = vars.get("PATH").map(OsString::from).unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(CARGO_EXE);
        if candidate.is_file() && !same_file(&candidate, self_exe) {
            return Some(candidate);
        }
    }
    let out = Command::new("rustup")
        .args(["which", "cargo"])
        .output()
        .ok()?;
    let found = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (out.status.success() && found.is_file() && !same_file(&found, self_exe)).then_some(found)
}

/// Runs the real cargo with `args`. On Unix this replaces the process; on Windows it waits and
/// returns cargo's exit code.
pub fn run(launch: &Launch, args: &[OsString]) -> Result<i32, String> {
    let mut cmd = Command::new(&launch.cargo);
    cmd.args(args)
        .env("CARGO_TARGET_DIR", &launch.target_dir)
        .env(DEPTH_VAR, launch.depth.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(format!("cannot run {}: {err}", launch.cargo.display()))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| format!("cannot run {}: {e}", launch.cargo.display()))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// The checked root for the real host, as `prepare` expects it.
pub fn host_check(vars: &Vars) -> Result<PathBuf, String> {
    crate::check::check_host(vars)
}

/// A check against a supplied probe, for callers and tests that need a seam.
pub fn probe_check<'a>(
    probe: &'a dyn VolumeProbe,
) -> impl FnOnce(&Vars) -> Result<PathBuf, String> + 'a {
    move |vars| crate::check::check(vars, probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::MARKER;
    use crate::testutil::Scratch;

    struct OnOwnVolume;
    impl VolumeProbe for OnOwnVolume {
        fn verify(&self, _root: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    fn setup(scratch: &Scratch) -> (PathBuf, PathBuf, PathBuf) {
        let root = scratch.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(MARKER), "dev-host-root v1\n").unwrap();
        let shim_dir = scratch.path().join("shim");
        let real_dir = scratch.path().join("real");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(shim_dir.join(CARGO_EXE), "shim").unwrap();
        std::fs::write(real_dir.join(CARGO_EXE), "real").unwrap();
        (root, shim_dir, real_dir)
    }

    fn vars(root: &Path, shim_dir: &Path, real_dir: &Path, extra: &[(&str, String)]) -> Vars {
        let path = std::env::join_paths([shim_dir, real_dir]).unwrap();
        let mut vars: Vars = [
            ("DEV_HOST_ROOT", root.display().to_string()),
            ("PATH", path.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .collect();
        for (k, v) in extra {
            vars.set(*k, v.clone());
        }
        vars
    }

    #[test]
    fn skips_itself_and_fills_in_a_slot() {
        let scratch = Scratch::new("shim-ok");
        let (root, shim_dir, real_dir) = setup(&scratch);
        let wt = scratch.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let v = vars(&root, &shim_dir, &real_dir, &[]);
        let launch = prepare(
            &v,
            &wt,
            &shim_dir.join(CARGO_EXE),
            &[],
            probe_check(&OnOwnVolume),
        )
        .unwrap();
        assert_eq!(launch.cargo, real_dir.join(CARGO_EXE));
        assert!(launch.target_dir.starts_with(root.join("build")));
        assert_eq!(launch.depth, 1);
    }

    #[test]
    fn refuses_target_dir_outside_root() {
        let scratch = Scratch::new("shim-outside");
        let (root, shim_dir, real_dir) = setup(&scratch);
        let outside = scratch.path().join("wt").join("target");
        let v = vars(
            &root,
            &shim_dir,
            &real_dir,
            &[("CARGO_TARGET_DIR", outside.display().to_string())],
        );
        let err = prepare(
            &v,
            scratch.path(),
            &shim_dir.join(CARGO_EXE),
            &[],
            probe_check(&OnOwnVolume),
        )
        .unwrap_err();
        assert!(err.contains("outside DEV_HOST_ROOT"), "{err}");
    }

    #[test]
    fn refuses_target_dir_argument_outside_root() {
        let scratch = Scratch::new("shim-arg");
        let (root, shim_dir, real_dir) = setup(&scratch);
        let wt = scratch.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let v = vars(&root, &shim_dir, &real_dir, &[]);
        let outside = scratch.path().join("outside").display().to_string();
        let inside = root.join("build/manual").display().to_string();
        let attempt = |args: &[&str]| {
            let args: Vec<OsString> = args.iter().map(OsString::from).collect();
            prepare(
                &v,
                &wt,
                &shim_dir.join(CARGO_EXE),
                &args,
                probe_check(&OnOwnVolume),
            )
        };
        for args in [
            vec!["build", "--target-dir", outside.as_str()],
            vec!["build", &format!("--target-dir={outside}")],
            vec!["build", "--target-dir", "../outside"],
            vec![
                "test",
                "--target-dir",
                &format!("{inside}/../../../outside"),
            ],
        ] {
            let err = attempt(&args).unwrap_err();
            assert!(err.contains("outside DEV_HOST_ROOT"), "{args:?}: {err}");
        }
        assert!(attempt(&["build", "--target-dir", &inside]).is_ok());
        assert!(attempt(&["build", &format!("--target-dir={inside}")]).is_ok());
        // After a bare `--` the flag belongs to the program cargo runs.
        assert!(attempt(&["run", "--", "--target-dir", &outside]).is_ok());
    }

    #[test]
    fn refuses_when_check_fails() {
        let scratch = Scratch::new("shim-check");
        let (root, shim_dir, real_dir) = setup(&scratch);
        std::fs::remove_file(root.join(MARKER)).unwrap();
        let v = vars(&root, &shim_dir, &real_dir, &[]);
        let err = prepare(
            &v,
            scratch.path(),
            &shim_dir.join(CARGO_EXE),
            &[],
            probe_check(&OnOwnVolume),
        )
        .unwrap_err();
        assert!(err.starts_with("dev-host check failed"), "{err}");
        let mut unset = v.clone();
        unset.set("DEV_HOST_ROOT", "");
        let err = prepare(
            &unset,
            scratch.path(),
            &shim_dir.join(CARGO_EXE),
            &[],
            probe_check(&OnOwnVolume),
        )
        .unwrap_err();
        assert!(err.contains("DEV_HOST_ROOT is not set"), "{err}");
    }

    #[test]
    fn refuses_runaway_nesting() {
        let scratch = Scratch::new("shim-depth");
        let (root, shim_dir, real_dir) = setup(&scratch);
        let v = vars(
            &root,
            &shim_dir,
            &real_dir,
            &[(DEPTH_VAR, MAX_DEPTH.to_string())],
        );
        let err = prepare(
            &v,
            scratch.path(),
            &shim_dir.join(CARGO_EXE),
            &[],
            probe_check(&OnOwnVolume),
        )
        .unwrap_err();
        assert!(err.contains("nested"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_shim_is_recognised_as_itself() {
        let scratch = Scratch::new("shim-link");
        let (_root, shim_dir, real_dir) = setup(&scratch);
        let binary = scratch.path().join("dev-host-bin");
        std::fs::write(&binary, "bin").unwrap();
        std::fs::remove_file(shim_dir.join(CARGO_EXE)).unwrap();
        std::os::unix::fs::symlink(&binary, shim_dir.join(CARGO_EXE)).unwrap();
        let path = std::env::join_paths([&shim_dir, &real_dir]).unwrap();
        let v: Vars = [("PATH", path.to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        assert_eq!(find_real_cargo(&v, &binary), Some(real_dir.join(CARGO_EXE)));
    }
}
