//! `dev-host cleanup`: keeps the root bounded (requirements E8 and E9).
//!
//! What it may delete, and nothing else:
//! - build slots `build/<project>/<key>/slot-<n>/` whose stamp file is older than `age_days`;
//! - entries of `t/` and `campaigns/<project>/<id>/work/` folders whose newest file (a deep
//!   scan of every file, not the folder's own mtime) is older than `age_days`;
//! - when `build/` or `t/` is over its budget, the least recently used idle entries of that
//!   folder until it is under budget, warm build slots last.
//!
//! `campaigns/*/*/evidence/`, `weights/`, `locks/` and every other folder are only measured.
//!
//! An entry is never deleted while a cargo `.cargo-lock` in it is held, while a live process
//! has a file in it open (or mapped, or running) or uses it as its working folder, or when the
//! scan could not read all of it. Build slots are deleted under their registry lock, so
//! `dev-host env` can't hand one out mid-delete.
//!
//! Live-process detection: Linux reads `/proc`; macOS and other Unix run `lsof` once per
//! cleanup and see only the processes the current user may inspect (run as the runner's user,
//! or as root to see everyone). Windows has no such scan here: it relies on Windows refusing to
//! delete a file another process has open without delete sharing, or a process's working
//! folder, so a delete there can stop part way and is reported as an error.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Budgets;
use crate::resolve;
use crate::slots::{self, BusyProbe};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Lists the paths live processes use.
pub trait Liveness {
    /// Every path a live process has open, mapped or running, or uses as its working folder.
    fn open_paths(&self) -> Result<Vec<PathBuf>, String>;

    /// A limitation to print with the report when this platform's check is partial.
    fn limitation(&self) -> Option<&'static str> {
        None
    }
}

/// The real process scan for this platform (see the module docs).
pub struct ProcessLiveness;

#[cfg(target_os = "linux")]
impl Liveness for ProcessLiveness {
    fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
        let procs = std::fs::read_dir("/proc").map_err(|e| format!("cannot read /proc: {e}"))?;
        let mut out = Vec::new();
        for entry in procs.flatten() {
            let pid_dir = entry.path();
            let is_pid = entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
            if !is_pid {
                continue;
            }
            for link in ["cwd", "exe", "root"] {
                if let Ok(target) = std::fs::read_link(pid_dir.join(link)) {
                    out.push(target);
                }
            }
            if let Ok(fds) = std::fs::read_dir(pid_dir.join("fd")) {
                out.extend(
                    fds.flatten()
                        .filter_map(|fd| std::fs::read_link(fd.path()).ok()),
                );
            }
            // A mapped file (a loaded library, an mmapped data file) needs no open descriptor.
            if let Ok(maps) = std::fs::read_to_string(pid_dir.join("maps")) {
                let mapped: HashSet<PathBuf> = mapped_paths(&maps).collect();
                out.extend(mapped);
            }
        }
        // `root` is "/" for nearly every process; it would make everything look live.
        out.retain(|p| p.parent().is_some());
        Ok(out)
    }
}

/// The files named in a `/proc/<pid>/maps` listing. Each line is `address perms offset dev
/// inode pathname`; only the pathname contains `/`, and it may contain spaces. A file deleted
/// while mapped carries a ` (deleted)` suffix.
#[cfg(any(target_os = "linux", test))]
fn mapped_paths(maps: &str) -> impl Iterator<Item = PathBuf> + '_ {
    maps.lines().filter_map(|line| {
        let path = &line[line.find('/')?..];
        Some(PathBuf::from(
            path.strip_suffix(" (deleted)").unwrap_or(path),
        ))
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
impl Liveness for ProcessLiveness {
    fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
        use std::process::{Command, Stdio};
        let mut last_err = String::new();
        // launchd jobs may run without /usr/sbin on PATH.
        for lsof in ["lsof", "/usr/sbin/lsof"] {
            let out = match Command::new(lsof)
                .args(["-w", "-n", "-P", "-F", "n"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                Ok(out) => out,
                Err(e) => {
                    last_err = format!("cannot run {lsof}: {e}");
                    continue;
                }
            };
            let text = String::from_utf8_lossy(&out.stdout);
            let paths: Vec<PathBuf> = text
                .lines()
                .filter_map(|line| line.strip_prefix('n'))
                .filter(|name| name.starts_with('/'))
                .map(PathBuf::from)
                .collect();
            if paths.is_empty() {
                return Err(format!("{lsof} listed no open files ({})", out.status));
            }
            return Ok(paths);
        }
        Err(last_err)
    }
}

#[cfg(windows)]
impl Liveness for ProcessLiveness {
    fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
        Ok(Vec::new())
    }

    fn limitation(&self) -> Option<&'static str> {
        Some(
            "Windows: no live-process scan; open files and working folders are protected only \
             by Windows refusing to delete them, so an in-use folder can be partly deleted \
             (reported as an error). Held cargo locks are still checked.",
        )
    }
}

/// Which part of the root an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Build,
    T,
    CampaignWork,
}

#[derive(Debug, Clone)]
pub struct Removed {
    pub path: PathBuf,
    pub bytes: u64,
    pub why: String,
}

#[derive(Debug, Clone)]
pub struct Skipped {
    pub path: PathBuf,
    pub why: String,
}

/// One line of the size report.
#[derive(Debug, Clone)]
pub struct Size {
    pub name: String,
    pub bytes: u64,
    pub budget: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub dry_run: bool,
    pub removed: Vec<Removed>,
    pub skipped: Vec<Skipped>,
    /// Sizes after this run's deletions (projected on a dry run).
    pub sizes: Vec<Size>,
    pub over_budget: Vec<String>,
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn render(&self) -> String {
        let verb = if self.dry_run {
            "would remove"
        } else {
            "removed"
        };
        let mut out = String::new();
        for r in &self.removed {
            out.push_str(&format!(
                "{verb} {} ({}, {})\n",
                r.path.display(),
                gb(r.bytes),
                r.why
            ));
        }
        if self.removed.is_empty() {
            out.push_str(&format!("{verb} nothing\n"));
        }
        for s in &self.skipped {
            out.push_str(&format!("kept {} ({})\n", s.path.display(), s.why));
        }
        if !self.sizes.is_empty() {
            out.push_str("sizes:\n");
            for s in &self.sizes {
                let budget = s
                    .budget
                    .map(|b| format!(" of {} budget", gb(b)))
                    .unwrap_or_default();
                out.push_str(&format!("  {:<18} {}{budget}\n", s.name, gb(s.bytes)));
            }
        }
        for o in &self.over_budget {
            out.push_str(&format!("over budget: {o}\n"));
        }
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        for e in &self.errors {
            out.push_str(&format!("error: {e}\n"));
        }
        out
    }
}

/// Bytes as GB (10^9) with two decimals.
pub fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// Bytes on disk and the newest file mtime under a path, without following symlinks.
#[derive(Debug, Clone, Copy)]
struct Usage {
    bytes: u64,
    newest_file: Option<SystemTime>,
    /// False when some part of the tree could not be read.
    complete: bool,
}

fn usage(path: &Path) -> Usage {
    let mut u = Usage {
        bytes: 0,
        newest_file: None,
        complete: true,
    };
    let mut seen = HashSet::new();
    add_usage(path, &mut u, &mut seen);
    u
}

fn add_usage(path: &Path, u: &mut Usage, seen: &mut HashSet<(u64, u64)>) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            u.complete = false;
            return;
        }
    };
    u.bytes += disk_bytes(&meta, seen);
    if meta.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => add_usage(&entry.path(), u, seen),
                        Err(_) => u.complete = false,
                    }
                }
            }
            Err(_) => u.complete = false,
        }
    } else if let Ok(modified) = meta.modified() {
        u.newest_file = Some(u.newest_file.map_or(modified, |n| n.max(modified)));
    }
}

/// Allocated bytes, counting each hard-linked file once per scan (cargo hard-links outputs).
#[cfg(unix)]
fn disk_bytes(meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn disk_bytes(meta: &std::fs::Metadata, _seen: &mut HashSet<(u64, u64)>) -> u64 {
    if meta.is_dir() { 0 } else { meta.len() }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(path).ok()?.modified().ok()
}

fn idle_for(last_used: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(last_used).unwrap_or(Duration::ZERO)
}

#[derive(Debug, Clone)]
struct Candidate {
    path: PathBuf,
    area: Area,
    /// `(key folder, slot number)` for build slots.
    slot: Option<(PathBuf, u32)>,
    bytes: u64,
    last_used: SystemTime,
    warm: bool,
    complete: bool,
    /// Set once removed or kept, so the budget pass doesn't retry it.
    settled: bool,
    /// Removed (or, on a dry run, would be).
    gone: bool,
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    out.sort();
    out
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// Whether `path` is a symlink (on Windows also a junction), which cleanup never follows.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether `path` itself lies strictly inside `root` (canonical): its parent, with every link
/// resolved, is `root` or below it. The last component is not resolved, since deleting a
/// symlink removes the link, not its target.
fn inside(path: &Path, root: &Path) -> bool {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .is_ok_and(|p| p.join(name).starts_with(root) && p.join(name) != root),
        _ => false,
    }
}

fn plain_candidate(path: PathBuf, area: Area) -> Candidate {
    let u = usage(&path);
    let last_used = u
        .newest_file
        .or_else(|| mtime(&path))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Candidate {
        path,
        area,
        slot: None,
        bytes: u.bytes,
        last_used,
        warm: false,
        complete: u.complete,
        settled: false,
        gone: false,
    }
}

/// Removes a file or folder (a symlink is removed, never followed).
fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn held_by_process(path: &Path, open: &[PathBuf]) -> bool {
    open.iter().any(|p| p.starts_with(path))
}

struct Run<'a> {
    budgets: &'a Budgets,
    /// The canonical root every deleted path must lie inside, when cleaning a root.
    confine: Option<PathBuf>,
    now: SystemTime,
    open: Vec<PathBuf>,
    busy: &'a dyn BusyProbe,
    report: Report,
}

impl Run<'_> {
    fn age_limit(&self) -> Duration {
        DAY * u32::try_from(self.budgets.age_days).unwrap_or(u32::MAX)
    }

    /// Whether `path` is a symlink, reported as kept: a link on the way to a candidate could
    /// lead outside the root.
    fn skip_link(&mut self, path: &Path) -> bool {
        let link = is_link(path);
        if link {
            self.report.skipped.push(Skipped {
                path: path.to_path_buf(),
                why: "a symlink; cleanup does not follow links".to_owned(),
            });
        }
        link
    }

    /// The entries of `dir`, or none when `dir` is a symlink.
    fn unlinked_children(&mut self, dir: &Path) -> Vec<PathBuf> {
        if self.skip_link(dir) {
            return Vec::new();
        }
        children(dir)
    }

    fn keep(&mut self, c: &mut Candidate, why: &str) {
        c.settled = true;
        self.report.skipped.push(Skipped {
            path: c.path.clone(),
            why: why.to_owned(),
        });
    }

    /// Deletes `c` (or records that it would) unless a liveness rule keeps it. With
    /// `recheck_age`, a build slot whose stamp was refreshed since the scan is kept.
    fn try_remove(&mut self, c: &mut Candidate, why: String, recheck_age: bool) -> bool {
        if let Some(root) = &self.confine
            && !inside(&c.path, root)
        {
            c.settled = true;
            self.report.errors.push(format!(
                "refusing {}: it resolves outside the dev-host root {}",
                c.path.display(),
                root.display()
            ));
            return false;
        }
        if !c.complete {
            self.keep(c, "could not read all of it");
            return false;
        }
        if held_by_process(&c.path, &self.open) {
            self.keep(c, "in use by a live process");
            return false;
        }
        let dry_run = self.report.dry_run;
        let outcome: Result<Option<&str>, String> = match &c.slot {
            Some(_) if dry_run => Ok(self
                .busy
                .is_busy(&c.path)
                .then_some("its cargo lock is held")),
            Some((key_dir, slot)) => {
                let busy = self.busy;
                let (now, limit) = (self.now, self.age_limit());
                let mut kept = None;
                slots::remove_slot(key_dir, *slot, &mut |dir| {
                    if busy.is_busy(dir) {
                        kept = Some("its cargo lock is held");
                        return Ok(false);
                    }
                    let stamp = mtime(&dir.join(slots::STAMP_FILE));
                    if recheck_age && stamp.is_some_and(|s| idle_for(s, now) <= limit) {
                        kept = Some("reassigned since the scan");
                        return Ok(false);
                    }
                    Ok(true)
                })
                .map(|_| kept)
            }
            None if self.busy.is_busy(&c.path) => Ok(Some("its cargo lock is held")),
            None if dry_run => Ok(None),
            None => remove_path(&c.path)
                .map(|()| None)
                .map_err(|e| format!("cannot remove {}: {e}", c.path.display())),
        };
        match outcome {
            Ok(None) => {
                c.settled = true;
                c.gone = true;
                self.report.removed.push(Removed {
                    path: c.path.clone(),
                    bytes: c.bytes,
                    why,
                });
                true
            }
            Ok(Some(reason)) => {
                self.keep(c, reason);
                false
            }
            Err(e) => {
                c.settled = true;
                self.report.errors.push(e);
                false
            }
        }
    }
}

/// Runs cleanup on `root`. `now` is the reference time for ages.
pub fn run(
    root: &Path,
    budgets: &Budgets,
    dry_run: bool,
    now: SystemTime,
    live: &dyn Liveness,
    busy: &dyn BusyProbe,
) -> Result<Report, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", root.display()))?;
    let open = live.open_paths().map_err(|e| {
        format!("cannot tell which folders live processes use ({e}); nothing was removed")
    })?;
    let mut run = Run {
        budgets,
        confine: Some(root.clone()),
        now,
        open,
        busy,
        report: Report {
            dry_run,
            ..Report::default()
        },
    };
    if let Some(note) = live.limitation() {
        run.report.notes.push(note.to_owned());
    }

    // Scan. A symlinked folder on the way to a candidate could lead outside the root, so no
    // level is followed through a link: the top-level areas, projects and campaigns included.
    let mut candidates = Vec::new();
    let mut build_other = 0u64;
    for project in run.unlinked_children(&root.join("build")) {
        if !is_real_dir(&project) {
            build_other += usage(&project).bytes;
            continue;
        }
        for key_dir in children(&project) {
            if !is_real_dir(&key_dir) {
                build_other += usage(&key_dir).bytes;
                continue;
            }
            let entries = match slots::entries(&key_dir) {
                Ok(entries) => entries,
                Err(e) => {
                    run.report.errors.push(e);
                    continue;
                }
            };
            for path in children(&key_dir) {
                let slot = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("slot-"))
                    .and_then(|n| n.parse::<u32>().ok());
                match slot {
                    Some(slot) if is_real_dir(&path) => {
                        let mut c = plain_candidate(path, Area::Build);
                        if let Some(stamp) = mtime(&c.path.join(slots::STAMP_FILE)) {
                            c.last_used = stamp;
                        }
                        c.warm = entries.iter().any(|e| e.slot == slot && e.warm);
                        c.slot = Some((key_dir.clone(), slot));
                        candidates.push(c);
                    }
                    _ => build_other += usage(&path).bytes,
                }
            }
        }
    }
    for path in run.unlinked_children(&root.join("t")) {
        candidates.push(plain_candidate(path, Area::T));
    }
    for project in run.unlinked_children(&root.join("campaigns")) {
        for campaign in run.unlinked_children(&project) {
            if run.skip_link(&campaign) {
                continue;
            }
            let work = campaign.join("work");
            if is_real_dir(&work) {
                candidates.push(plain_candidate(work, Area::CampaignWork));
            }
        }
    }
    let campaigns_total = usage(&root.join("campaigns")).bytes;

    // E8: age.
    let limit = run.age_limit();
    for c in candidates.iter_mut() {
        let idle = idle_for(c.last_used, now);
        if idle > limit {
            let why = format!("idle {} days", idle.as_secs() / DAY.as_secs());
            run.try_remove(c, why, true);
        }
    }

    // E9: budgets for build/ and t/.
    for (area, budget, other) in [
        (Area::Build, budgets.build, build_other),
        (Area::T, budgets.t, 0),
    ] {
        let Some(budget) = budget else { continue };
        let mut total = bytes_in(&candidates, area, false) + other;
        if total <= budget {
            continue;
        }
        let mut order: Vec<usize> = (0..candidates.len())
            .filter(|&i| candidates[i].area == area && !candidates[i].settled)
            .collect();
        order.sort_by_key(|&i| (candidates[i].warm, candidates[i].last_used));
        for i in order {
            if total <= budget {
                break;
            }
            let c = &mut candidates[i];
            let why = if c.warm {
                "over budget, warm slot".to_owned()
            } else {
                "over budget, least recently used".to_owned()
            };
            if run.try_remove(c, why, false) {
                total = total.saturating_sub(c.bytes);
            }
        }
    }

    // Report sizes after deletions.
    let work_total = bytes_in(&candidates, Area::CampaignWork, false);
    let mut sizes = Vec::new();
    let mut loose = 0u64;
    for path in children(&root) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !is_real_dir(&path) {
            loose += usage(&path).bytes;
            continue;
        }
        let (bytes, budget) = match name.as_str() {
            "build" => (
                bytes_in(&candidates, Area::Build, false) + build_other,
                budgets.build,
            ),
            "t" => (bytes_in(&candidates, Area::T, false), budgets.t),
            "campaigns" => (
                campaigns_total.saturating_sub(bytes_in(&candidates, Area::CampaignWork, true)),
                None,
            ),
            "cache" => (usage(&path).bytes, budgets.cache),
            "docker" => (usage(&path).bytes, budgets.docker),
            _ => (usage(&path).bytes, None),
        };
        sizes.push(Size {
            name: format!("{name}/"),
            bytes,
            budget,
        });
        if name == "campaigns" {
            sizes.push(Size {
                name: "campaigns/*/*/work".to_owned(),
                bytes: work_total,
                budget: budgets.campaigns_work,
            });
        }
    }
    if loose > 0 {
        sizes.push(Size {
            name: "(files at root)".to_owned(),
            bytes: loose,
            budget: None,
        });
    }
    for s in &sizes {
        if let Some(budget) = s.budget
            && s.bytes > budget
        {
            run.report.over_budget.push(format!(
                "{} is {} against a {} budget",
                s.name,
                gb(s.bytes),
                gb(budget)
            ));
        }
    }
    run.report.sizes = sizes;
    Ok(run.report)
}

/// Total bytes of `area`'s candidates that are gone (`gone`) or still there (`!gone`).
fn bytes_in(candidates: &[Candidate], area: Area, gone: bool) -> u64 {
    candidates
        .iter()
        .filter(|c| c.area == area && c.gone == gone)
        .map(|c| c.bytes)
        .sum()
}

/// `cleanup --migrate`: deletes an old copy outside the root under the same liveness rules.
///
/// Refuses a symlink, a filesystem root, `home` or anything containing it, the root itself,
/// anything containing the root, and anything inside the root (cleanup manages that).
pub fn migrate(
    root: &Path,
    target: &Path,
    home: Option<&Path>,
    cwd: &Path,
    dry_run: bool,
    live: &dyn Liveness,
    busy: &dyn BusyProbe,
) -> Result<Report, String> {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd.join(target)
    };
    let shown = target.display();
    match std::fs::symlink_metadata(&absolute) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "{shown} is a symlink; remove the link itself, or name the folder it points to"
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("{shown}: {e}")),
    }
    // The target exists, so resolve it as the OS will open it: `a/link/../b` is the `b` beside
    // the link's target, not `a/b`.
    let path = absolute
        .canonicalize()
        .map_err(|e| format!("cannot resolve {shown}: {e}"))?;
    let root = resolve(root, cwd);
    if path.parent().is_none() {
        return Err(format!("refusing {shown}: it is a filesystem root"));
    }
    if path == root || root.starts_with(&path) {
        return Err(format!(
            "refusing {shown}: it is or contains the dev-host root {}",
            root.display()
        ));
    }
    if path.starts_with(&root) {
        return Err(format!(
            "refusing {shown}: it is inside the dev-host root; `dev-host cleanup` manages that"
        ));
    }
    if let Some(home) = home {
        let home = resolve(home, cwd);
        if home.starts_with(&path) {
            return Err(format!(
                "refusing {shown}: it is or contains the home folder {}",
                home.display()
            ));
        }
    }
    let open = live.open_paths().map_err(|e| {
        format!("cannot tell which folders live processes use ({e}); nothing was removed")
    })?;
    let mut run = Run {
        budgets: &Budgets::default(),
        confine: None,
        now: SystemTime::now(),
        open,
        busy,
        report: Report {
            dry_run,
            ..Report::default()
        },
    };
    if let Some(note) = live.limitation() {
        run.report.notes.push(note.to_owned());
    }
    let mut c = plain_candidate(path, Area::T);
    run.try_remove(&mut c, "migrated copy".to_owned(), false);
    Ok(run.report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slots::{CARGO_LOCK, CargoLockProbe};
    use crate::testutil::Scratch;
    use std::fs::{File, OpenOptions};

    struct NoProcesses;
    impl Liveness for NoProcesses {
        fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
            Ok(Vec::new())
        }
    }

    struct Listed(Vec<PathBuf>);
    impl Liveness for Listed {
        fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
            Ok(self.0.clone())
        }
    }

    struct Broken;
    impl Liveness for Broken {
        fn open_paths(&self) -> Result<Vec<PathBuf>, String> {
            Err("no lsof".into())
        }
    }

    fn days_ago(days: u64) -> SystemTime {
        SystemTime::now() - DAY * u32::try_from(days).unwrap()
    }

    fn write_aged(path: &Path, bytes: usize, at: SystemTime) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    /// A slot under `<root>/build/demo/key/` with a payload file, stamped `stamp_age` days ago.
    fn slot(root: &Path, worktree: &str, warm: bool, stamp_age: u64, bytes: usize) -> PathBuf {
        let key = root.join("build/demo/key");
        let dir = slots::assign(&key, worktree, 8, 0, warm, &CargoLockProbe).unwrap();
        write_aged(&dir.join("debug/payload"), bytes, days_ago(stamp_age));
        set_mtime(&dir.join(slots::STAMP_FILE), days_ago(stamp_age));
        dir
    }

    fn budgets(f: impl FnOnce(&mut Budgets)) -> Budgets {
        let mut b = Budgets::from_config(&Default::default()).unwrap();
        f(&mut b);
        b
    }

    fn root(scratch: &Scratch) -> PathBuf {
        let root = scratch.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn age_rule_removes_stale_entries_and_keeps_protected_folders() {
        let scratch = Scratch::new("cleanup-age");
        let root = root(&scratch);
        let stale_slot = slot(&root, "/w/old", false, 8, 10);
        let fresh_slot = slot(&root, "/w/new", false, 1, 10);
        // Directory mtime is fresh, but the newest file is 8 days old: deep scan wins.
        write_aged(&root.join("t/oldtmp01/a/b/f"), 10, days_ago(8));
        write_aged(&root.join("t/newtmp01/a/f"), 10, days_ago(8));
        write_aged(&root.join("t/newtmp01/g"), 10, days_ago(2));
        write_aged(&root.join("campaigns/p/c1/work/x"), 10, days_ago(9));
        write_aged(&root.join("campaigns/p/c1/evidence/x"), 10, days_ago(90));
        write_aged(&root.join("weights/p/w.bin"), 10, days_ago(90));
        write_aged(&root.join("locks/apple-metal.lock"), 0, days_ago(90));

        let report = run(
            &root,
            &budgets(|_| {}),
            false,
            SystemTime::now(),
            &NoProcesses,
            &CargoLockProbe,
        )
        .unwrap();
        assert!(!stale_slot.exists());
        assert!(fresh_slot.exists());
        assert!(!root.join("t/oldtmp01").exists());
        assert!(root.join("t/newtmp01").exists());
        assert!(!root.join("campaigns/p/c1/work").exists());
        assert!(root.join("campaigns/p/c1/evidence/x").exists());
        assert!(root.join("weights/p/w.bin").exists());
        assert!(root.join("locks/apple-metal.lock").exists());
        assert_eq!(report.removed.len(), 3, "{}", report.render());
        assert!(report.errors.is_empty(), "{}", report.render());
        let entries = slots::entries(&root.join("build/demo/key")).unwrap();
        assert_eq!(entries.len(), 1, "the removed slot leaves the registry");
        let text = report.render();
        for name in [
            "build/",
            "t/",
            "campaigns/",
            "campaigns/*/*/work",
            "weights/",
            "locks/",
        ] {
            assert!(text.contains(name), "size line for {name}: {text}");
        }
    }

    #[test]
    fn dry_run_deletes_nothing() {
        let scratch = Scratch::new("cleanup-dry");
        let root = root(&scratch);
        let stale_slot = slot(&root, "/w/old", false, 8, 10);
        write_aged(&root.join("t/oldtmp01/f"), 10, days_ago(8));
        let report = run(
            &root,
            &budgets(|_| {}),
            true,
            SystemTime::now(),
            &NoProcesses,
            &CargoLockProbe,
        )
        .unwrap();
        assert_eq!(report.removed.len(), 2);
        assert!(stale_slot.exists());
        assert!(root.join("t/oldtmp01/f").exists());
        assert!(report.render().contains("would remove"));
    }

    #[test]
    fn held_cargo_lock_and_listed_processes_keep_stale_entries() {
        let scratch = Scratch::new("cleanup-live");
        let root = root(&scratch);
        let locked_slot = slot(&root, "/w/locked", false, 8, 10);
        let lock_path = locked_slot.join("debug").join(CARGO_LOCK);
        std::fs::write(&lock_path, "").unwrap();
        let held = OpenOptions::new().write(true).open(&lock_path).unwrap();
        held.lock().unwrap();
        write_aged(&root.join("t/inuse001/f"), 10, days_ago(8));
        let report = run(
            &root,
            &budgets(|_| {}),
            false,
            SystemTime::now() + DAY,
            &Listed(vec![root.join("t/inuse001/f")]),
            &CargoLockProbe,
        )
        .unwrap();
        assert!(locked_slot.exists());
        assert!(root.join("t/inuse001/f").exists());
        assert!(report.removed.is_empty(), "{}", report.render());
        let text = report.render();
        assert!(text.contains("cargo lock is held"), "{text}");
        assert!(text.contains("live process"), "{text}");
        drop(held);
    }

    #[test]
    fn real_process_scan_sees_an_open_file_and_a_working_folder() {
        let scratch = Scratch::new("cleanup-proc");
        let root = root(&scratch);
        write_aged(&root.join("t/openfile/f"), 10, days_ago(8));
        write_aged(&root.join("t/cwdhere1/f"), 10, days_ago(8));
        write_aged(&root.join("t/idle0001/f"), 10, days_ago(8));
        let _open = File::open(root.join("t/openfile/f")).unwrap();
        #[cfg(unix)]
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .current_dir(root.join("t/cwdhere1"))
            .spawn()
            .unwrap();
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1 >NUL"])
            .current_dir(root.join("t/cwdhere1"))
            .spawn()
            .unwrap();
        let report = run(
            &root,
            &budgets(|_| {}),
            false,
            SystemTime::now(),
            &ProcessLiveness,
            &CargoLockProbe,
        );
        let _ = child.kill();
        let _ = child.wait();
        let report = report.unwrap();
        assert!(!root.join("t/idle0001").exists(), "{}", report.render());
        // Windows has no process scan; the protection there is Windows refusing the delete.
        if cfg!(unix) {
            assert!(root.join("t/openfile/f").exists(), "{}", report.render());
            assert!(root.join("t/cwdhere1").exists(), "{}", report.render());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_process_scan_sees_a_mapped_file() {
        // A copy of a shared library this test binary uses, preloaded into a child: the loader
        // maps it and closes its descriptor, so only /proc/<pid>/maps shows it.
        let scratch = Scratch::new("cleanup-maps");
        let root = root(&scratch);
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let Some(lib) = mapped_paths(&maps).find(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.contains(".so") && !name.starts_with("libc.") && !name.starts_with("ld-")
        }) else {
            eprintln!("no shared library other than libc is mapped; nothing to preload");
            return;
        };
        let copy = root.join("t/mapped01/lib.so");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&lib, &copy).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .env("LD_PRELOAD", &copy)
            .current_dir(scratch.path())
            .spawn()
            .unwrap();
        let seen = (0..100).any(|_| {
            let found = ProcessLiveness.open_paths().unwrap().contains(&copy);
            if !found {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            found
        });
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            seen,
            "{} mapped by the child was not listed",
            copy.display()
        );
    }

    #[test]
    fn mapped_paths_read_proc_maps_lines() {
        let maps = "\
55d0a000-55d0b000 r--p 00000000 08:01 1234                       /usr/bin/sleep
7f00-7f01 r-xp 00001000 08:01 99   /data/root/t/ab cd/lib.so
7f01-7f02 rw-p 00000000 00:00 0 
7f02-7f03 rw-s 00000000 00:05 7    /data/root/t/gone/f (deleted)
7ffd-7ffe rw-p 00000000 00:00 0                          [stack]
";
        let paths: Vec<PathBuf> = mapped_paths(maps).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/usr/bin/sleep"),
                PathBuf::from("/data/root/t/ab cd/lib.so"),
                PathBuf::from("/data/root/t/gone/f"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folders_are_never_followed() {
        let scratch = Scratch::new("cleanup-links");
        let root = root(&scratch);
        let outside = scratch.path().join("outside");
        let link = |target: &Path, at: &Path| {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, at).unwrap();
        };
        // build/ itself, with a stale slot behind it.
        let slot_dir = outside.join("build/demo/key/slot-0");
        write_aged(&slot_dir.join("debug/payload"), 10, days_ago(9));
        write_aged(&slot_dir.join(slots::STAMP_FILE), 1, days_ago(9));
        link(&outside.join("build"), &root.join("build"));
        // t/ itself.
        write_aged(&outside.join("t/old00001/f"), 10, days_ago(9));
        link(&outside.join("t"), &root.join("t"));
        // A campaign project, and a campaign.
        write_aged(&outside.join("project/c1/work/x"), 10, days_ago(9));
        link(&outside.join("project"), &root.join("campaigns/p1"));
        write_aged(&outside.join("campaign/work/x"), 10, days_ago(9));
        link(&outside.join("campaign"), &root.join("campaigns/p2/c2"));

        let report = run(
            &root,
            &budgets(|b| {
                b.build = Some(0);
                b.t = Some(0);
            }),
            false,
            SystemTime::now(),
            &NoProcesses,
            &CargoLockProbe,
        )
        .unwrap();
        assert!(report.removed.is_empty(), "{}", report.render());
        for kept in [
            slot_dir.join("debug/payload"),
            outside.join("t/old00001/f"),
            outside.join("project/c1/work/x"),
            outside.join("campaign/work/x"),
        ] {
            assert!(kept.exists(), "{} was deleted", kept.display());
        }
        for linked in ["build", "t", "campaigns/p1", "campaigns/p2/c2"] {
            assert!(
                report.skipped.iter().any(|s| s.path == root.join(linked)),
                "{linked}: {}",
                report.render()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn deletion_is_confined_to_paths_whose_parent_resolves_inside_the_root() {
        let scratch = Scratch::new("cleanup-inside");
        let root = root(&scratch);
        let outside = scratch.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(root.join("t")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("t/link")).unwrap();
        assert!(inside(&root.join("t/a"), &root));
        // The link itself is inside (removing it removes the link); what is behind it is not.
        assert!(inside(&root.join("t/link"), &root));
        assert!(!inside(&root.join("t/link/a"), &root));
        assert!(!inside(&root, &root));
        assert!(!inside(&outside.join("a"), &root));
    }

    #[test]
    fn failed_process_scan_removes_nothing() {
        let scratch = Scratch::new("cleanup-broken");
        let root = root(&scratch);
        write_aged(&root.join("t/old00001/f"), 10, days_ago(8));
        let err = run(
            &root,
            &budgets(|_| {}),
            false,
            SystemTime::now(),
            &Broken,
            &CargoLockProbe,
        )
        .unwrap_err();
        assert!(err.contains("nothing was removed"), "{err}");
        assert!(root.join("t/old00001/f").exists());
    }

    #[test]
    fn over_budget_build_evicts_least_recent_idle_slots_warm_last() {
        let scratch = Scratch::new("cleanup-budget");
        let root = root(&scratch);
        let mb = 1_000_000;
        let warm = slot(&root, "/w/warm", true, 3, mb);
        let older = slot(&root, "/w/older", false, 2, mb);
        let newer = slot(&root, "/w/newer", false, 1, mb);
        let busy = slot(&root, "/w/busy", false, 4, mb);
        let lock_path = busy.join("debug").join(CARGO_LOCK);
        std::fs::write(&lock_path, "").unwrap();
        let held = OpenOptions::new().write(true).open(&lock_path).unwrap();
        held.lock().unwrap();
        // Room for about two slots: the busy one stays, so one more may stay: the warm one.
        let report = run(
            &root,
            &budgets(|b| b.build = Some(2_500_000)),
            false,
            SystemTime::now(),
            &NoProcesses,
            &CargoLockProbe,
        )
        .unwrap();
        drop(held);
        let text = report.render();
        assert!(busy.exists(), "{text}");
        assert!(!older.exists(), "{text}");
        assert!(!newer.exists(), "{text}");
        assert!(warm.exists(), "warm goes last: {text}");
        let order: Vec<_> = report.removed.iter().map(|r| r.path.clone()).collect();
        assert_eq!(order, vec![older, newer], "{text}");
        assert!(text.contains("GB budget"), "{text}");
    }

    #[test]
    fn over_budget_t_evicts_oldest_first_and_reports_when_still_over() {
        let scratch = Scratch::new("cleanup-tbudget");
        let root = root(&scratch);
        let mb = 1_000_000;
        write_aged(&root.join("t/aaaa0001/f"), mb, days_ago(3));
        write_aged(&root.join("t/bbbb0002/f"), mb, days_ago(1));
        write_aged(&root.join("t/cccc0003/f"), mb, days_ago(2));
        let report = run(
            &root,
            &budgets(|b| b.t = Some(1_500_000)),
            false,
            SystemTime::now(),
            &NoProcesses,
            &CargoLockProbe,
        )
        .unwrap();
        assert!(!root.join("t/aaaa0001").exists());
        assert!(!root.join("t/cccc0003").exists());
        assert!(root.join("t/bbbb0002").exists());
        assert!(report.over_budget.is_empty(), "{}", report.render());

        write_aged(&root.join("t/dddd0004/f"), 3 * mb, days_ago(1));
        let _open = File::open(root.join("t/dddd0004/f")).unwrap();
        let report = run(
            &root,
            &budgets(|b| b.t = Some(1_500_000)),
            false,
            SystemTime::now(),
            &Listed(vec![root.join("t/dddd0004/f")]),
            &CargoLockProbe,
        )
        .unwrap();
        assert!(root.join("t/dddd0004").exists());
        assert_eq!(report.over_budget.len(), 1, "{}", report.render());
    }

    #[test]
    fn migrate_deletes_an_old_copy_and_refuses_dangerous_paths() {
        let scratch = Scratch::new("cleanup-migrate");
        let root = root(&scratch);
        let home = scratch.path().join("home");
        let old = home.join("old-target");
        write_aged(&old.join("debug/x"), 10, days_ago(1));
        let m = |p: &Path, dry| {
            migrate(
                &root,
                p,
                Some(&home),
                scratch.path(),
                dry,
                &NoProcesses,
                &CargoLockProbe,
            )
        };
        assert_eq!(m(&old, true).unwrap().removed.len(), 1);
        assert!(old.exists());
        for bad in [
            PathBuf::from("/"),
            home.clone(),
            scratch.path().to_path_buf(),
            root.clone(),
            root.join("t"),
            scratch.path().join("missing"),
        ] {
            assert!(m(&bad, false).is_err(), "{} must be refused", bad.display());
        }
        #[cfg(unix)]
        {
            let link = scratch.path().join("link");
            std::os::unix::fs::symlink(&old, &link).unwrap();
            assert!(m(&link, false).unwrap_err().contains("symlink"));
        }
        #[cfg(unix)]
        {
            // `left/link/../victim` is the victim beside the link's target, not `left/victim`.
            let left = scratch.path().join("left");
            let right = scratch.path().join("right");
            write_aged(&left.join("victim/f"), 10, days_ago(1));
            write_aged(&right.join("victim/f"), 10, days_ago(1));
            std::fs::create_dir_all(right.join("sub")).unwrap();
            std::os::unix::fs::symlink(right.join("sub"), left.join("link")).unwrap();
            let report = m(&left.join("link/../victim"), false).unwrap();
            assert_eq!(report.removed.len(), 1, "{}", report.render());
            assert_eq!(report.removed[0].path, right.join("victim"));
            assert!(!right.join("victim").exists());
            assert!(left.join("victim/f").exists());
        }
        let held = old.join("debug").join(CARGO_LOCK);
        std::fs::write(&held, "").unwrap();
        let lock = OpenOptions::new().write(true).open(&held).unwrap();
        lock.lock().unwrap();
        let report = m(&old, false).unwrap();
        assert!(report.removed.is_empty());
        assert!(old.exists());
        drop(lock);
        // A process another test forks can briefly share the lock's open file; wait it out.
        assert!((0..50).any(|_| {
            let idle = !CargoLockProbe.is_busy(&old);
            if !idle {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            idle
        }));
        let report = migrate(
            &root,
            &old,
            Some(&home),
            scratch.path(),
            false,
            &Listed(vec![old.join("debug")]),
            &CargoLockProbe,
        )
        .unwrap();
        assert!(report.removed.is_empty());
        let report = m(&old, false).unwrap();
        assert_eq!(report.removed.len(), 1, "{}", report.render());
        assert!(!old.exists());
    }
}
