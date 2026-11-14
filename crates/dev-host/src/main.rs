//! `dev-host` command line. See the library docs for what each subcommand does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dev_host::cleanup::{self, ProcessLiveness};
use dev_host::config::{self, Budgets};
use dev_host::guard::{self, Rules};
use dev_host::plan::{self, Format, Request};
use dev_host::slots::CargoLockProbe;
use dev_host::{Vars, check, shim};

const USAGE: &str = "\
usage:
  dev-host check
      Verify DEV_HOST_ROOT: set, on its own volume, with the .dev-host-root marker.
  dev-host env [--format sh|github|powershell] [--project NAME] [--worktree PATH]
               [--target TRIPLE] [--session ID] [--slots N] [--warm]
      Print CARGO_TARGET_DIR (sticky build slot) and TMPDIR (TEMP/TMP on Windows).
      --warm marks this worktree's slot warm: reassigned and evicted last (the 7-day idle
      rule still applies).
        sh          eval \"$(dev-host env)\"
        github      dev-host env --format github >> \"$GITHUB_ENV\"
        powershell  dev-host env --format powershell | Out-String | Invoke-Expression
                    (default on Windows; for GitHub Actions on Windows use --format github
                    >> $env:GITHUB_ENV)
  dev-host cargo [--] ARGS...
      Run the real cargo after the checks. Installing dev-host under the name `cargo`
      ahead of the real one on PATH does the same.
  dev-host cleanup [--dry-run]
      Delete build slots whose stamp is older than age_days, and t/<id> and
      campaigns/*/*/work entries whose newest file is older than age_days; then, while
      build/ or t/ is over budget, delete their least recently used idle entries (warm
      slots last). Never touches campaigns/*/*/evidence, weights/ or locks/, and never
      deletes an entry whose cargo lock is held or that a live process has open or as its
      working folder (macOS: lsof, Linux: /proc; Windows has no process scan and relies on
      Windows refusing to delete open files). Prints what it removed and the size of each
      top-level folder. Settings in <root>/dev-host.conf (key = value):
        age_days (default 7), build_budget_gb, t_budget_gb, cache_budget_gb,
        campaigns_work_budget_gb, docker_budget_gb (GB = 10^9 bytes; unset = no budget)
  dev-host cleanup --migrate PATH [--dry-run]
      Delete an old copy outside the root under the same liveness checks. Refuses a
      symlink, a filesystem root, $HOME or a folder containing it, the root, a folder
      containing the root, and anything inside the root.
  dev-host guard (--diff BASE | --all | --docs) [--repo DIR] [--path GLOB]...
                 [--allow-prefix PATH]...
      --diff BASE  check lines added since the merge base with BASE (needs full history)
      --all        check every line of every tracked file
      Both check files matching --path (default: .github/** **/scripts/** **/tools/**,
      docs *.md excluded) and fail, with file:line and a reason, on: user app data
      (~/SceneWorks, ~/Library/Application Support/SceneWorks); an absolute-path cargo
      launch; CARGO_TARGET_DIR or --target-dir set to a value not mentioning DEV_HOST_ROOT;
      a <repo>/target assumption (./target/, target/release, $X/target, join(.., \"target\"));
      a machine path (/Users/, /Volumes/, /tmp, /private/tmp, ~/, $HOME/, ${HOME},
      %USERPROFILE%, %PUBLIC%, Path.home(), C:\\..., E:\\...). Exempt: lines with
      `dev-host: allow-path <reason>`, comment-only lines, and paths under
      /opt/homebrew/bin, /usr/bin, /usr/local/bin, /bin, C:\\Windows, C:\\Program Files or an
      --allow-prefix.
      --docs       AGENTS.md must hold a section between <!-- dev-host --> and
                   <!-- /dev-host --> (FEATURE_DEVELOPMENT.md may hold one too); together
                   they must contain: DEV_HOST_ROOT; the layout build/ t/ cache/ weights/
                   campaigns/ locks/; `dev-host env`; `dev-host: allow-path`; and the
                   rule never to search from / or $HOME (\"never search\" with $HOME).
      Exit 0 when clean, 1 on findings, 2 on usage or git errors.

environment: DEV_HOST_ROOT (required except for guard), DEV_HOST_SLOTS, DEV_HOST_SESSION,
             DEV_HOST_REAL_CARGO
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
        "cleanup" => run_cleanup(&vars, &rest[1..]),
        "guard" => run_guard(&rest[1..]),
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
        if flag == "--warm" {
            req.warm = true;
            continue;
        }
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

fn usage_error(message: &str) -> ExitCode {
    eprint!("{message}\n{USAGE}");
    ExitCode::from(2)
}

fn run_cleanup(vars: &Vars, args: &[OsString]) -> ExitCode {
    let mut dry_run = false;
    let mut migrate: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.to_string_lossy().as_ref() {
            "--dry-run" => dry_run = true,
            "--migrate" => match it.next() {
                Some(path) => migrate = Some(PathBuf::from(path)),
                None => return usage_error("--migrate needs a path"),
            },
            other => return usage_error(&format!("unknown option {other}")),
        }
    }
    let root = match check::check_host(vars) {
        Ok(root) => root,
        Err(e) => return fail(&format!("dev-host check failed: {e}")),
    };
    let budgets = match config::read(&root).and_then(|c| Budgets::from_config(&c)) {
        Ok(budgets) => budgets,
        Err(e) => return fail(&format!("dev-host cleanup: {e}")),
    };
    let report = match migrate {
        Some(path) => {
            let home = vars
                .get("HOME")
                .or_else(|| vars.get("USERPROFILE"))
                .map(PathBuf::from);
            current_dir().and_then(|cwd| {
                cleanup::migrate(
                    &root,
                    &path,
                    home.as_deref(),
                    &cwd,
                    dry_run,
                    &ProcessLiveness,
                    &CargoLockProbe,
                )
            })
        }
        None => cleanup::run(
            &root,
            &budgets,
            dry_run,
            std::time::SystemTime::now(),
            &ProcessLiveness,
            &CargoLockProbe,
        ),
    };
    match report {
        Ok(report) => {
            print!("{}", report.render());
            if report.errors.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => fail(&format!("dev-host cleanup: {e}")),
    }
}

enum GuardMode {
    Diff(String),
    All,
    Docs,
}

fn run_guard(args: &[OsString]) -> ExitCode {
    let mut mode = None;
    let mut repo: Option<PathBuf> = None;
    let mut globs: Vec<String> = Vec::new();
    let mut prefixes: Vec<String> = Vec::new();
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(flag) = it.next() {
        let next_mode = match flag.as_str() {
            "--all" => Some(GuardMode::All),
            "--docs" => Some(GuardMode::Docs),
            "--diff" | "--repo" | "--path" | "--allow-prefix" => {
                let Some(value) = it.next() else {
                    return usage_error(&format!("{flag} needs a value"));
                };
                match flag.as_str() {
                    "--diff" => Some(GuardMode::Diff(value)),
                    "--repo" => {
                        repo = Some(PathBuf::from(value));
                        None
                    }
                    "--path" => {
                        globs.push(value);
                        None
                    }
                    _ => {
                        prefixes.push(value);
                        None
                    }
                }
            }
            other => return usage_error(&format!("unknown option {other}")),
        };
        if let Some(m) = next_mode {
            if mode.is_some() {
                return usage_error("give one of --diff, --all, --docs");
            }
            mode = Some(m);
        }
    }
    let Some(mode) = mode else {
        return usage_error("give one of --diff, --all, --docs");
    };
    let dir = match repo.map(Ok).unwrap_or_else(current_dir) {
        Ok(dir) => dir,
        Err(e) => return fail(&e),
    };
    let top = match guard::repo_top(&dir) {
        Ok(top) => top,
        Err(e) => {
            eprintln!("dev-host guard: {e}");
            return ExitCode::from(2);
        }
    };
    if globs.is_empty() {
        globs = guard::DEFAULT_PATHS.iter().map(|g| g.to_string()).collect();
    }
    let rules = Rules::new(&prefixes);
    let findings = match mode {
        GuardMode::Docs => {
            let missing = guard::check_docs(&top);
            for item in &missing {
                println!("{}: {item}", top.join("AGENTS.md").display());
            }
            return if missing.is_empty() {
                println!("dev-host guard: agent docs carry every dev-host item");
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        GuardMode::Diff(base) => guard::check_diff(&top, &base, &globs, &rules),
        GuardMode::All => guard::check_all(&top, &globs, &rules),
    };
    match findings {
        Ok(findings) if findings.is_empty() => {
            println!("dev-host guard: no new hardcoded paths");
            ExitCode::SUCCESS
        }
        Ok(findings) => {
            for f in &findings {
                print!("{}", f.render());
            }
            let files: std::collections::BTreeSet<&str> =
                findings.iter().map(|f| f.file.as_str()).collect();
            println!(
                "dev-host guard: {} finding(s) in {} file(s); fix them or mark a deliberate \
                 path with `dev-host: allow-path <reason>`",
                findings.len(),
                files.len()
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("dev-host guard: {e}");
            ExitCode::from(2)
        }
    }
}
