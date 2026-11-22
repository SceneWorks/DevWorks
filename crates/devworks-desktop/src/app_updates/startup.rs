//! One-shot helper and candidate startup handshake. A normal startup cannot arm an install.
use super::{
    BUILD, Installation, Journal, Stage, bundled_help_digest, digest_executable, exchange,
    native_error,
};
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::update::{
    compatible_predecessor, stopped_profile, verify_recovery_store,
};
use devworks_core::fingerprint::{is_lower_hex, sha256_hex};
use devworks_core::ports::process::{NativeIdentity, Probe, ProcessHost};
use devworks_core::ports::update::{InstallApproval, InstallOwner};
use std::fs::{File, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELPER: &str = "--internal-app-install";
const CANDIDATE: &str = "--internal-app-startup";
const READY: &str = "DevWorks startup verified";
const ADMIT: &str = "DevWorks startup approved";
const WINDOW_OPEN: &str = "DevWorks window available";
const COMPLETE: &str = "DevWorks update recorded";
const HELPER_BOUND: &str = "DevWorks updater bound";
const RECOVER: &str = "--resume-update-recovery";

pub struct ApplicationStartup {
    pub gate: Option<File>,
    pub candidate: Option<Candidate>,
    pub recovery_profile: Option<PathBuf>,
}

pub struct Candidate {
    install: Installation,
    journal: Journal,
    gate: Option<File>,
}

impl Candidate {
    pub fn profile(&self) -> &Path {
        &self.journal.profile
    }

    pub fn approval(&self) -> &InstallApproval {
        &self.journal.approval
    }

    /// Called after the signed schema gate and engine construction. No runtime recovery,
    /// delivery or user work starts until the authenticated helper releases the gate.
    pub fn admit(&mut self) -> Result<(), String> {
        send_line(io::stdout().lock(), READY)?;
        if read_line(io::stdin().lock())? != ADMIT {
            return Err(
                "The updater did not admit this launch. Reopen DevWorks to inspect recovery."
                    .into(),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let gate = loop {
            match self.install.gate(false) {
                Ok(gate) => break gate,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(gate_error(error)),
            }
        };
        let found = self
            .install
            .journal()?
            .ok_or("The installer journal disappeared.")?;
        if found.stage != Stage::ActivationAllowed || found.transaction != self.journal.transaction
        {
            return Err("The signed update approval changed before admission.".into());
        }
        self.install.verify_running()?;
        self.gate = Some(gate);
        Ok(())
    }

    /// Verify the native frame and record terminal health before admitting UI commands.
    pub fn healthy(&self) -> Result<(), String> {
        if self.gate.is_none() {
            return Err("The updated app never entered the installed gate.".into());
        }
        send_line(io::stdout().lock(), WINDOW_OPEN)?;
        if read_line(io::stdin().lock())? != COMPLETE {
            return Err(
                "The updater did not commit this launch. Reopen DevWorks to inspect recovery."
                    .into(),
            );
        }
        let found = self
            .install
            .journal()?
            .ok_or("The installer journal disappeared.")?;
        if found.stage != Stage::Healthy || found.transaction != self.journal.transaction {
            return Err("The healthy update record changed before admission.".into());
        }
        self.install.verify_running()?;
        Ok(())
    }

    pub fn take_gate(&mut self) -> Option<File> {
        self.gate.take()
    }
}

pub fn enter_application() -> Result<ApplicationStartup, String> {
    if recovery_requested() {
        return open_recovery();
    }
    let candidate_id = internal_arg(CANDIDATE)?;
    if let Some(id) = candidate_id {
        return open_candidate(Installation::locate()?, &id);
    }
    let Some(install) = Installation::normal_entry()? else {
        return Ok(ApplicationStartup {
            gate: None,
            candidate: None,
            recovery_profile: None,
        });
    };
    install.verify_running()?;
    let gate = install.gate(false).map_err(gate_error)?;
    if let Some(found) = install.journal()?
        && (found.stage.outcome().is_none() || found.stage == Stage::RecoveryRequired)
    {
        drop(gate);
        recover(&install, found)?;
        let gate = install.gate(false).map_err(gate_error)?;
        install.verify_running()?;
        return Ok(ApplicationStartup {
            gate: Some(gate),
            candidate: None,
            recovery_profile: None,
        });
    }
    install.verify_running()?;
    Ok(ApplicationStartup {
        gate: Some(gate),
        candidate: None,
        recovery_profile: None,
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
    let cookie = read_line(io::stdin().lock())?;
    let journal = install
        .journal()?
        .ok_or("The installer has no startup journal.")?;
    let peer = journal
        .installer_pid
        .zip(journal.installer_birth.as_ref())
        .map(|(pid, birth)| NativeIdentity {
            pid,
            birth: birth.clone(),
            group: None,
        });
    if journal.transaction != id
        || journal.stage != Stage::StoreStartupIntent
        || journal.candidate_cookie_digest.as_ref() != Some(&sha256_hex(cookie.as_bytes()))
        || !peer
            .as_ref()
            .is_some_and(|peer| NativeProcessHost.has_parent(peer))
    {
        return Err("The installer has no matching live candidate startup authority.".into());
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
        candidate: Some(Candidate {
            install,
            journal,
            gate: None,
        }),
        recovery_profile: None,
    })
}

pub(super) fn spawn_helper(
    install: &Installation,
    mut journal: Journal,
    entry: &Path,
    cookie: &str,
) -> Result<(), String> {
    let old_root = if install.owner == InstallOwner::WindowsPerUser {
        entry
            .parent()
            .ok_or("The previous Windows image is invalid.")?
    } else {
        &install.root
    };
    exchange::platform_identity(install.owner, old_root)?;
    install.publish(&journal)?;
    let started = (|| {
        // Execute the verified installed image before replacement. The helper needs no path
        // back to its code after startup. On macOS this preserves its original bundle signature.
        let child = Command::new(entry)
            .args([HELPER, journal.transaction.as_str()])
            .current_dir(&install.control)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
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
            cookie,
        )?;
        let receiver = read_child(&mut child, 1)?;
        expect_message(&receiver, HELPER_BOUND, Duration::from_secs(8))?;
        child.detach();
        Ok::<_, String>(())
    })();
    if let Err(error) = &started {
        journal.record(install, Stage::Unchanged, error)?;
    }
    started
}

fn run_helper(id: &str) -> Result<(), String> {
    let cookie = read_line(io::stdin().lock())?;
    let install = Installation::locate()?;
    let mut journal = install
        .journal()?
        .ok_or("The updater has no approved journal.")?;
    let parent = NativeIdentity {
        pid: journal.parent,
        birth: journal.parent_birth.clone(),
        group: None,
    };
    if journal.transaction != id
        || journal.stage != Stage::ApprovedShutdown
        || journal.helper_cookie_digest != sha256_hex(cookie.as_bytes())
        || !NativeProcessHost.has_parent(&parent)
        || digest_executable(&install.verify_running()?)? != journal.old_digest
        || BUILD != Some(&journal.approval.old_build)
    {
        return Err("The updater does not match the approved build, process and shutdown.".into());
    }
    send_line(io::stdout().lock(), HELPER_BOUND)?;
    let mut gate = Some(await_stopped(&install, &journal)?);
    let on_disk = install
        .journal()?
        .ok_or("The approved updater journal disappeared.")?;
    if on_disk.transaction != journal.transaction || on_disk.stage != Stage::ApprovedShutdown {
        return Err("The pending update was already handled.".into());
    }
    if let Err(error) = apply(&install, &mut journal, &mut gate) {
        let _recovery_gate = if gate.is_none() {
            Some(install.gate(true).map_err(gate_error)?)
        } else {
            None
        };
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

fn apply(
    install: &Installation,
    journal: &mut Journal,
    gate: &mut Option<File>,
) -> Result<(), String> {
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
    let peer = NativeProcessHost
        .current_identity()
        .ok_or("The updater's native process identity is unavailable.")?;
    let cookie = devworks_adapters::app_updates::random_token().map_err(|error| error.message)?;
    journal.installer_pid = Some(peer.pid);
    journal.installer_birth = Some(peer.birth);
    journal.candidate_cookie_digest = Some(sha256_hex(cookie.as_bytes()));
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
    send_line(&mut input, &cookie)?;
    let receiver = read_child(&mut child, 2)?;
    expect_message(&receiver, READY, Duration::from_secs(120))?;
    journal.record(
        install,
        Stage::ActivationAllowed,
        "The approved store and engine initialization passed. DevWorks may start its runtime.",
    )?;
    drop(gate.take());
    send_line(&mut input, ADMIT)?;
    // Runtime owners may activate after admission. A lost health channel now requires
    // their normal shutdown or explicit recovery, never a forced child kill.
    child.1 = true;
    expect_message(&receiver, WINDOW_OPEN, Duration::from_secs(120))?;
    // The app holds the shared install gate throughout the final observation. After health
    // succeeds the committed installed authority survives a broken response pipe.
    journal.record(install, Stage::Healthy, "The updated build opened the existing workspace with matching Help. Held work needs Continue.")?;
    let _ = send_line(input, COMPLETE);
    Ok(())
}

fn read_child(
    child: &mut Launched,
    lines: usize,
) -> Result<std::sync::mpsc::Receiver<Result<String, String>>, String> {
    let output = child
        .0
        .stdout
        .take()
        .ok_or("The updater has no private response channel.")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(lines);
    std::thread::spawn(move || {
        let mut output = BufReader::new(output);
        for _ in 0..lines {
            if sender.send(read_line(&mut output)).is_err() {
                return;
            }
        }
    });
    Ok(receiver)
}

fn expect_message(
    receiver: &std::sync::mpsc::Receiver<Result<String, String>>,
    expected: &str,
    wait: Duration,
) -> Result<(), String> {
    let seen = receiver
        .recv_timeout(wait)
        .map_err(|_| "DevWorks did not report the expected updater milestone.")??;
    if seen != expected {
        return Err("The private updater milestone did not match the staged request.".into());
    }
    Ok(())
}

fn recovery_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsStr::new(RECOVER)])
}

fn open_recovery() -> Result<ApplicationStartup, String> {
    let install = Installation::normal_entry()?
        .ok_or("Recovery requires the installed DevWorks entrypoint.")?;
    let gate = install.gate(true).map_err(gate_error)?;
    let entry = install.verify_running()?;
    let mut journal = install
        .journal()?
        .ok_or("There is no interrupted app update to recover.")?;
    if journal.stage != Stage::RecoveryRequired {
        return Err("The current app update does not require manual recovery.".into());
    }
    if install.owner == InstallOwner::LinuxStandalone {
        if digest_executable(&entry)? != journal.approval.stage.manifest.entrypoint().content.sha256
        {
            return Err(
                "This standalone installation has no verified release witness for recovery.".into(),
            );
        }
        use devworks_core::ports::update::ReleaseSource;
        super::source(&journal.profile)
            .verify(&journal.approval.stage, &Default::default())
            .map_err(|error| error.message)?;
    } else {
        let root = if install.owner == InstallOwner::WindowsPerUser {
            entry
                .parent()
                .ok_or("The Windows recovery code has no version directory.")?
        } else {
            &install.root
        };
        exchange::platform_identity(install.owner, root)?;
    }
    let profile_lock =
        verify_recovery_store(&journal.profile, &journal.approval).map_err(|e| e.to_string())?;
    journal.record(&install, Stage::Recovered, "The installer did not confirm startup. The user opened the preserved profile with a verified compatible build.")?;
    let profile = journal.profile;
    drop(profile_lock);
    gate.unlock().map_err(native_error)?;
    let gate = install.gate(false).map_err(gate_error)?;
    install.verify_running()?;
    Ok(ApplicationStartup {
        gate: Some(gate),
        candidate: None,
        recovery_profile: Some(profile),
    })
}

fn recover(install: &Installation, observed: Journal) -> Result<(), String> {
    let gate = install.gate(true).map_err(gate_error)?;
    let Some(mut journal) = install.journal()? else {
        return Err("The interrupted updater's journal disappeared.".into());
    };
    if observed.transaction != journal.transaction {
        return Err("The installation changed while recovery was being inspected.".into());
    }
    if journal.stage == Stage::RecoveryRequired {
        return Err(format!(
            "{} To open with a verified compatible build, launch the installed app with {RECOVER}.",
            journal.detail
        ));
    }
    if journal.stage.outcome().is_none() {
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
        if journal.stage == Stage::ActivationAllowed
            && journal
                .installer_pid
                .zip(journal.installer_birth.as_ref())
                .is_some_and(|(pid, birth)| {
                    !matches!(
                        NativeProcessHost.probe(&NativeIdentity {
                            pid,
                            birth: birth.clone(),
                            group: None
                        }),
                        Probe::Gone
                    )
                })
        {
            return Err("DevWorks is still verifying the updated runtime.".into());
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
    if journal.stage == Stage::ActivationAllowed {
        return journal.record(install, Stage::RecoveryRequired, "The new app entered runtime recovery before startup was interrupted. The installed version and workspace were kept.");
    }
    let current = install
        .canonical_entry()
        .and_then(|entry| digest_executable(&entry));
    match current {
        Ok(digest) if digest == journal.old_digest => {
            let old_authority = compatible_predecessor(&journal.profile, &journal.approval);
            if old_authority.is_ok() {
                exchange::remove_unused(install, journal);
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
    let gate = if !recovery_requested()
        && let Some(found) = install.journal()?
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

pub(super) fn gate_error(error: TryLockError) -> String {
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
