//! One-shot helper and candidate startup handshake. A normal startup cannot arm an install.
use super::{
    BUILD, Installation, Journal, Stage, bundled_help_digest, digest_executable, exchange,
    native_error,
};
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::update::{compatible_predecessor, stopped_profile};
use devworks_core::fingerprint::is_lower_hex;
use devworks_core::ports::process::{NativeIdentity, Probe, ProcessHost};
use devworks_core::ports::update::{InstallApproval, InstallOwner};
use std::fs::{File, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELPER: &str = "--internal-app-install";
const CANDIDATE: &str = "--internal-app-startup";
const READY: &str = "DevWorks startup verified";
const ADMIT: &str = "DevWorks startup approved";

pub struct ApplicationStartup {
    pub gate: Option<File>,
    pub candidate: Option<Candidate>,
}

pub struct Candidate {
    install: Installation,
    journal: Journal,
}

impl Candidate {
    pub fn profile(&self) -> &Path {
        &self.journal.profile
    }

    pub fn approval(&self) -> &InstallApproval {
        &self.journal.approval
    }

    /// The desktop calls this once its native window and engine have started. The current
    /// helper commits the health effect before releasing the lifetime installation gate.
    pub fn ready(self) -> Result<File, String> {
        send_line(io::stdout().lock(), READY)?;
        if read_line(io::stdin().lock())? != ADMIT {
            return Err(
                "The updater did not commit this launch. Restart DevWorks to inspect recovery."
                    .into(),
            );
        }
        let gate = self.install.gate(false).map_err(gate_error)?;
        let found = self
            .install
            .journal()?
            .ok_or("The installer journal disappeared.")?;
        if found.stage != Stage::Healthy || found.transaction != self.journal.transaction {
            return Err("The healthy update record changed before admission.".into());
        }
        self.install.verify_running()?;
        Ok(gate)
    }
}

pub fn enter_application() -> Result<ApplicationStartup, String> {
    let candidate_id = internal_arg(CANDIDATE)?;
    let install = match Installation::locate() {
        Ok(install) => install,
        Err(error) if candidate_id.is_some() => return Err(error),
        Err(_) => {
            return Ok(ApplicationStartup {
                gate: None,
                candidate: None,
            });
        }
    };
    if let Some(id) = candidate_id {
        return open_candidate(install, &id);
    }
    install.verify_running()?;
    let gate = install.gate(false).map_err(gate_error)?;
    if let Some(found) = install.journal()?
        && (found.stage.outcome().is_none() || found.stage == Stage::RecoveryRequired)
    {
        drop(gate);
        recover(&install, found)?;
        return Ok(ApplicationStartup {
            gate: Some(install.gate(false).map_err(gate_error)?),
            candidate: None,
        });
    }
    install.verify_running()?;
    Ok(ApplicationStartup {
        gate: Some(gate),
        candidate: None,
    })
}

pub fn run_internal() -> Option<Result<(), String>> {
    match internal_arg(HELPER) {
        Ok(Some(id)) => Some(run_helper(&id)),
        Err(error) => Some(Err(error)),
        Ok(None) => {
            #[cfg(windows)]
            if let Ok(install) = Installation::locate()
                && install.owner == InstallOwner::WindowsPerUser
                && std::env::current_exe()
                    .is_ok_and(|path| path == install.root.join("DevWorks.exe"))
            {
                return Some(windows_launcher(install));
            }
            None
        }
    }
}

fn internal_arg(marker: &str) -> Result<Option<String>, String> {
    let mut args = std::env::args_os().skip(1);
    let Some(first) = args.next() else {
        return Ok(None);
    };
    if first != marker {
        return Ok(None);
    }
    let value = args
        .next()
        .and_then(|arg| arg.into_string().ok())
        .filter(|value| is_lower_hex(value, 32))
        .ok_or("The private update startup argument is invalid.")?;
    if args.next().is_some() {
        return Err("The private update startup has unexpected arguments.".into());
    }
    Ok(Some(value))
}

fn open_candidate(install: Installation, id: &str) -> Result<ApplicationStartup, String> {
    if read_line(io::stdin().lock())? != id {
        return Err("The update candidate did not receive its private startup handoff.".into());
    }
    let journal = install
        .journal()?
        .ok_or("The installer has no startup journal.")?;
    if journal.transaction != id || journal.stage != Stage::StoreStartupIntent {
        return Err("The installer has no matching candidate startup authority.".into());
    }
    let manifest = &journal.approval.stage.manifest;
    let entry = install.verify_running()?;
    if BUILD != Some(&manifest.build)
        || manifest.tag.strip_prefix('v') != Some(crate::VERSION)
        || manifest.help_digest != bundled_help_digest()
        || manifest.target != devworks_adapters::packages::host_target_triple()
        || digest_executable(&entry)? != manifest.entrypoint().content.sha256
    {
        return Err("The new application build, executable or bundled Help differs from the signed approval.".into());
    }
    // The helper still owns the exclusive installation gate. The normal startup path may
    // never proceed with this handoff unless the verified health receipt releases that gate.
    if !matches!(install.gate(false), Err(TryLockError::WouldBlock)) {
        return Err("The update candidate requires the live installer startup gate.".into());
    }
    Ok(ApplicationStartup {
        gate: None,
        candidate: Some(Candidate { install, journal }),
    })
}

pub(super) fn spawn_helper(
    install: &Installation,
    journal: &Journal,
    entry: &Path,
) -> Result<(), String> {
    let old_root = if install.owner == InstallOwner::WindowsPerUser {
        entry
            .parent()
            .ok_or("The previous Windows image is invalid.")?
    } else {
        &install.root
    };
    exchange::platform_identity(install.owner, old_root)?;
    install.save(journal)?;
    // Execute the verified installed image before replacement. The helper needs no path
    // back to its code after startup. On macOS this preserves the original bundle signature.
    let child = Command::new(entry)
        .args([HELPER, journal.transaction.as_str()])
        .current_dir(&install.control)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(native_error)?;
    let mut child = Launched::new(child);
    send_line(
        child
            .0
            .stdin
            .take()
            .ok_or("The updater has no private input.")?,
        &journal.transaction,
    )?;
    child.detach();
    Ok(())
}

fn run_helper(id: &str) -> Result<(), String> {
    if read_line(io::stdin().lock())? != id {
        return Err("The updater did not receive a private startup handoff.".into());
    }
    let install = Installation::locate()?;
    let mut journal = install
        .journal()?
        .ok_or("The updater has no approved journal.")?;
    if journal.transaction != id
        || journal.stage != Stage::ApprovedShutdown
        || digest_executable(&install.verify_running()?)? != journal.old_digest
        || BUILD != Some(&journal.approval.old_build)
    {
        return Err("The updater does not match the approved build and shutdown.".into());
    }
    let gate = await_stopped(&install, &journal)?;
    let on_disk = install
        .journal()?
        .ok_or("The approved updater journal disappeared.")?;
    if on_disk.transaction != journal.transaction || on_disk.stage != Stage::ApprovedShutdown {
        return Err("The pending update was already handled.".into());
    }
    if let Err(error) = apply(&install, &mut journal, &gate) {
        reconcile_failure(&install, &mut journal, &error)?;
    }
    Ok(())
}

fn await_stopped(install: &Installation, journal: &Journal) -> Result<File, String> {
    let process = NativeIdentity {
        pid: journal.parent,
        birth: journal.parent_birth.clone(),
        group: None,
    };
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match NativeProcessHost.probe(&process) {
            Probe::Gone => {
                return install.gate(true).map_err(gate_error);
            }
            Probe::Alive if Instant::now() < deadline => {}
            _ => {
                return Err(
                    "The updater could not verify that the approved DevWorks process exited."
                        .into(),
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn apply(install: &Installation, journal: &mut Journal, gate: &File) -> Result<(), String> {
    let profile =
        stopped_profile(&journal.profile, &journal.approval).map_err(|error| error.to_string())?;
    let prepared = exchange::prepare(install, journal)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    if now >= journal.approval.expires_at_ms {
        return Err("The install approval expired before replacement.".into());
    }
    journal.record(
        install,
        Stage::ReplaceIntent,
        "The signed update is ready for atomic replacement.",
    )?;
    exchange::activate(install, journal, &prepared)?;
    journal.record(
        install,
        Stage::LaunchIntent,
        "The installed address now selects the signed candidate.",
    )?;
    drop(profile);
    let entry = install.canonical_entry()?;
    journal.record(
        install,
        Stage::StoreStartupIntent,
        "The updated app may verify and start the approved workspace.",
    )?;
    let child = Command::new(entry)
        .args([CANDIDATE, &journal.transaction])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(native_error)?;
    let mut child = Launched::new(child);
    let mut input = child
        .0
        .stdin
        .take()
        .ok_or("The updated app has no private input.")?;
    send_line(&mut input, &journal.transaction)?;
    let output = child
        .0
        .stdout
        .take()
        .ok_or("The updated app has no health channel.")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(read_line(BufReader::new(output)));
    });
    let message = receiver
        .recv_timeout(Duration::from_secs(120))
        .map_err(|_| "The updated app did not report a healthy desktop startup.")??;
    if message != READY {
        return Err("The updated app did not confirm the signed startup handoff.".into());
    }
    journal.record(install, Stage::Healthy, "The updated build opened the existing workspace with matching Help. Held work needs Continue.")?;
    if let Err(error) = gate
        .unlock()
        .map_err(native_error)
        .and_then(|()| send_line(input, ADMIT))
    {
        // Healthy startup and the new authority are already committed. Do not roll back
        // after that observation, including a broken channel between commit and release.
        eprintln!("devworks updater: {error}");
        child.detach();
        return Ok(());
    }
    child.detach();
    Ok(())
}

fn recover(install: &Installation, observed: Journal) -> Result<(), String> {
    let gate = install.gate(true).map_err(gate_error)?;
    let Some(mut journal) = install.journal()? else {
        return Err("The interrupted updater's journal disappeared.".into());
    };
    if observed.transaction != journal.transaction {
        return Err("The installation changed while recovery was being inspected.".into());
    }
    if journal.stage.outcome().is_none() || journal.stage == Stage::RecoveryRequired {
        if matches!(
            NativeProcessHost.probe(&NativeIdentity {
                pid: journal.parent,
                birth: journal.parent_birth.clone(),
                group: None,
            }),
            Probe::Alive | Probe::Unknown(_)
        ) {
            return Err("The approved update process has not exited. Close the other DevWorks window before reopening.".into());
        }
        reconcile_failure(
            install,
            &mut journal,
            "The previous installer was interrupted.",
        )?;
    }
    let _ = gate.unlock();
    match journal.stage {
        Stage::Healthy | Stage::Unchanged => Ok(()),
        Stage::RolledBack => {
            Err("DevWorks restored a compatible previous app. Reopen DevWorks to continue.".into())
        }
        _ => Err(journal.detail),
    }
}

fn reconcile_failure(
    install: &Installation,
    journal: &mut Journal,
    reason: &str,
) -> Result<(), String> {
    if journal.stage == Stage::Healthy {
        return Ok(());
    }
    let current = install
        .canonical_entry()
        .and_then(|entry| digest_executable(&entry));
    match current {
        Ok(digest) if digest == journal.old_digest => {
            let old_authority = compatible_predecessor(&journal.profile, &journal.approval);
            if old_authority.is_ok() {
                let stage = if matches!(
                    journal.stage,
                    Stage::LaunchIntent | Stage::StoreStartupIntent | Stage::RecoveryRequired
                ) {
                    Stage::RolledBack
                } else {
                    Stage::Unchanged
                };
                return journal.record(install, stage, reason);
            }
        }
        Ok(digest) if digest == journal.approval.stage.manifest.entrypoint().content.sha256 => {
            if let Ok(_profile) = compatible_predecessor(&journal.profile, &journal.approval) {
                match exchange::rollback(install, journal) {
                    Ok(()) => return journal.record(install, Stage::RolledBack,
                        &format!("{reason} A compatible previous app was restored. Workspace data was kept.")),
                    Err(error) => return journal.record(install, Stage::RecoveryRequired,
                        &format!("{reason} Previous-app recovery could not be verified. {error}")),
                }
            }
        }
        _ => {}
    }
    journal.record(install, Stage::RecoveryRequired,
        &format!("{reason} The current workspace or installation could not be proved compatible with the previous app. Keep this profile and reopen with a compatible build."))
}

#[cfg(windows)]
fn windows_launcher(install: Installation) -> Result<(), String> {
    let gate = install.gate(false).map_err(gate_error)?;
    let gate = if let Some(found) = install.journal()?
        && (found.stage.outcome().is_none() || found.stage == Stage::RecoveryRequired)
    {
        drop(gate);
        recover(&install, found)?;
        install.gate(false).map_err(gate_error)?
    } else {
        gate
    };
    let entry = install.canonical_entry()?;
    exchange::windows_signature(&entry)?;
    Command::new(entry)
        .args(std::env::args_os().skip(1))
        .spawn()
        .map_err(native_error)?;
    drop(gate);
    Ok(())
}

fn read_line(read: impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    BufReader::new(read.take(129))
        .read_line(&mut line)
        .map_err(native_error)?;
    if line.len() > 128 || !line.ends_with('\n') {
        return Err("A private updater handoff was interrupted or invalid.".into());
    }
    Ok(line.trim_end_matches('\n').to_owned())
}

fn send_line(mut sink: impl Write, value: &str) -> Result<(), String> {
    writeln!(sink, "{value}")
        .and_then(|()| sink.flush())
        .map_err(native_error)
}

fn gate_error(error: TryLockError) -> String {
    match error {
        TryLockError::WouldBlock => "Another DevWorks process owns this installation. Close other DevWorks windows before retrying.".into(),
        TryLockError::Error(error) => native_error(error),
    }
}

struct Launched(Child, bool);
impl Launched {
    fn new(child: Child) -> Self {
        Self(child, false)
    }
    fn detach(mut self) {
        self.1 = true;
    }
}
impl Drop for Launched {
    fn drop(&mut self) {
        if !self.1 {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
