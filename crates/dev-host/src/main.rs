//! `dev-host` command line. See the library docs for what each subcommand does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dev_host::plan::{self, Format, Request};
use dev_host::{Vars, check, shim};

const USAGE: &str = "\
usage:
  dev-host check
      Verify DEV_HOST_ROOT: set, on its own volume, with the .dev-host-root marker.
  dev-host env [--format sh|github|powershell] [--project NAME] [--worktree PATH]
               [--target TRIPLE] [--session ID] [--slots N]
      Print CARGO_TARGET_DIR (sticky build slot) and TMPDIR (TEMP/TMP on Windows).
        sh          eval \"$(dev-host env)\"
        github      dev-host env --format github >> \"$GITHUB_ENV\"
        powershell  dev-host env --format powershell | Out-String | Invoke-Expression
                    (default on Windows; for GitHub Actions on Windows use --format github
                    >> $env:GITHUB_ENV)
  dev-host cargo [--] ARGS...
      Run the real cargo after the checks. Installing dev-host under the name `cargo`
      ahead of the real one on PATH does the same.

environment: DEV_HOST_ROOT (required), DEV_HOST_SLOTS, DEV_HOST_SESSION, DEV_HOST_REAL_CARGO
";

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let rest: Vec<OsString> = args.collect();
    let vars = Vars::from_process();

    let invoked_as_cargo = Path::new(&argv0)
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("cargo"));
    if invoked_as_cargo {
        return run_shim(&vars, &rest);
    }

    let Some(command) = rest.first().and_then(|c| c.to_str()) else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    match command {
        "check" if rest.len() == 1 => match check::check_host(&vars) {
            Ok(root) => {
                println!("dev-host root ok: {}", root.display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("dev-host check failed: {e}")),
        },
        "env" => run_env(&vars, &rest[1..]),
        "cargo" => {
            let tail = &rest[1..];
            let tail = if tail.first().is_some_and(|a| a == "--") {
                &tail[1..]
            } else {
                tail
            };
            run_shim(&vars, tail)
        }
        "--version" | "-V" => {
            println!("dev-host {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}

fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot read the current folder: {e}"))
}

fn run_env(vars: &Vars, args: &[OsString]) -> ExitCode {
    let mut format = Format::default_for_platform();
    let mut req = Request::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let flag = flag.to_string_lossy();
        let Some(value) = it.next().map(|v| v.to_string_lossy().into_owned()) else {
            eprint!("{flag} needs a value\n{USAGE}");
            return ExitCode::from(2);
        };
        match flag.as_ref() {
            "--format" => match Format::parse(&value) {
                Some(f) => format = f,
                None => {
                    eprint!("unknown format {value}\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--project" => req.project = Some(value),
            "--worktree" => req.worktree = Some(PathBuf::from(value)),
            "--target" => req.target = Some(value),
            "--session" => req.session = Some(value),
            "--slots" => match value.parse() {
                Ok(n) => req.slots = Some(n),
                Err(_) => {
                    eprint!("--slots needs a whole number\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            _ => {
                eprint!("unknown option {flag}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let root = match check::check_host(vars) {
        Ok(root) => root,
        Err(e) => return fail(&format!("dev-host check failed: {e}")),
    };
    let result = current_dir()
        .and_then(|cwd| plan::assignments(&root, vars, &cwd, &req))
        .and_then(|a| plan::render(&a, format));
    match result {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("dev-host env: {e}")),
    }
}

fn run_shim(vars: &Vars, args: &[OsString]) -> ExitCode {
    let launch = std::env::current_exe()
        .map_err(|e| format!("cannot locate this program: {e}"))
        .and_then(|exe| current_dir().map(|cwd| (exe, cwd)))
        .and_then(|(exe, cwd)| shim::prepare(vars, &cwd, &exe, args, shim::host_check));
    let launch = match launch {
        Ok(launch) => launch,
        Err(e) => return fail(&format!("dev-host cargo shim: refusing to run cargo: {e}")),
    };
    match shim::run(&launch, args) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => fail(&format!("dev-host cargo shim: {e}")),
    }
}
