//! The ordinary installed entrypoint remains discoverable across every replacement.
use super::{
    Installation, Journal, create_private_dir, digest_executable, native_error, source,
    sync_directory,
};
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::update::InstallOwner;
use std::io;
use std::path::{Path, PathBuf};

pub(super) fn prepare(install: &Installation, journal: &Journal) -> Result<PathBuf, String> {
    let path = install.prepared_root(&journal.transaction);
    create_private_dir(&path).map_err(native_error)?;
    let result = source(&journal.profile)
        .materialize(&journal.approval.stage, &path, &Cancellation::default())
        .map_err(|error| error.message)
        .and_then(|()| platform_identity(install.owner, &path));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&path);
    }
    result?;
    sync_directory(&install.control)?;
    Ok(path)
}

pub(super) fn activate(
    install: &Installation,
    journal: &Journal,
    prepared: &Path,
) -> Result<(), String> {
    let manifest = &journal.approval.stage.manifest;
    if digest_executable(&install.canonical_entry()?)? != journal.old_digest
        || digest_executable(&prepared.join(install.owner.entrypoint()))?
            != manifest.entrypoint().content.sha256
    {
        return Err("The old or prepared application changed before replacement.".into());
    }
    if install.owner == InstallOwner::WindowsPerUser {
        let key = &journal.approval.stage.manifest_digest;
        let next = install.root.join("versions").join(key);
        move_new(prepared, &next).map_err(native_error)?;
        install.replace_private(&install.root, "current", key.as_bytes())
    } else {
        swap(&install.root, prepared).map_err(native_error)?;
        sync_parents(&install.root, prepared)
    }
}

pub(super) fn rollback(install: &Installation, journal: &Journal) -> Result<(), String> {
    if digest_executable(&install.canonical_entry()?)?
        != journal.approval.stage.manifest.entrypoint().content.sha256
    {
        return Err("The installed candidate changed and cannot be rolled back safely.".into());
    }
    let prepared = install.prepared_root(&journal.transaction);
    if install.owner == InstallOwner::WindowsPerUser {
        let old_key = &journal.old_pointer;
        if !devworks_core::fingerprint::is_sha256_hex(old_key) {
            return Err("The previous Windows installation pointer is invalid.".into());
        }
        let previous = install.root.join("versions").join(old_key);
        verify_predecessor(install.owner, &previous, &journal.old_digest)?;
        install.replace_private(&install.root, "current", old_key.as_bytes())
    } else {
        verify_predecessor(install.owner, &prepared, &journal.old_digest)?;
        swap(&install.root, &prepared).map_err(native_error)?;
        sync_parents(&install.root, &prepared)
    }
}

/// The old normal address was verified. Retire only this transaction's prepared
/// candidate and only when its main executable still matches the signed stage.
pub(super) fn remove_unused(install: &Installation, journal: &Journal) {
    let prepared = install.prepared_root(&journal.transaction);
    let candidate = prepared.join(install.owner.entrypoint());
    if super::private_to_owner(&prepared, true).unwrap_or(false)
        && digest_executable(&candidate).is_ok_and(|actual| {
            actual == journal.approval.stage.manifest.entrypoint().content.sha256
        })
    {
        let _ = std::fs::remove_dir_all(&prepared);
        let _ = sync_directory(&install.control);
    }
}

pub(super) fn retire_previous(install: &Installation, journal: &Journal) {
    if journal.stage != super::Stage::Healthy {
        remove_unused(install, journal);
        return;
    }
    let retired = match install.owner {
        InstallOwner::WindowsPerUser
            if devworks_core::fingerprint::is_sha256_hex(&journal.old_pointer) =>
        {
            install.root.join("versions").join(&journal.old_pointer)
        }
        InstallOwner::WindowsPerUser => return,
        _ => install.prepared_root(&journal.transaction),
    };
    let entry = retired.join(install.owner.entrypoint());
    if install
        .canonical_entry()
        .is_ok_and(|current| current != entry)
        && super::private_to_owner(&retired, true).unwrap_or(false)
        && digest_executable(&entry).is_ok_and(|digest| digest == journal.old_digest)
    {
        let _ = std::fs::remove_dir_all(&retired);
        if let Some(parent) = retired.parent() {
            let _ = sync_directory(parent);
        }
    }
}

fn verify_predecessor(owner: InstallOwner, path: &Path, digest: &str) -> Result<(), String> {
    if digest_executable(&path.join(owner.entrypoint()))? != digest {
        return Err(
            "The previous application failed identity verification. Its files were retained."
                .into(),
        );
    }
    platform_identity(owner, path)
}

fn sync_parents(first: &Path, second: &Path) -> Result<(), String> {
    sync_directory(first.parent().ok_or("The installed app has no parent.")?)?;
    sync_directory(second.parent().ok_or("The staged app has no parent.")?)
}

#[cfg(target_os = "macos")]
fn swap(first: &Path, second: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    // SAFETY: C strings outlive the call, and both paths are verified owner-controlled roots.
    if unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_SWAP,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn swap(first: &Path, second: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    // SAFETY: C strings outlive the call, and both paths are verified owner-controlled roots.
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn swap(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(windows)]
pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    move_file(from, to, true)
}

#[cfg(not(windows))]
pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn move_new(from: &Path, to: &Path) -> io::Result<()> {
    move_file(from, to, false)
}

#[cfg(not(windows))]
fn move_new(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, replace: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (from, to) = (wide(from), wide(to));
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            Default::default()
        };
    // SAFETY: both paths are owned, NUL-terminated UTF-16 strings for the call.
    unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), flags) }
        .map_err(|error| io::Error::other(error.to_string()))
}

pub(super) fn platform_identity(owner: InstallOwner, path: &Path) -> Result<(), String> {
    match owner {
        InstallOwner::LinuxStandalone if cfg!(target_os = "linux") => Ok(()),
        InstallOwner::MacApplication => mac_signature(path),
        InstallOwner::WindowsPerUser => windows_signature(&path.join(owner.entrypoint())),
        _ => Err("This installation owner is unqualified on this platform.".into()),
    }
}

#[cfg(target_os = "macos")]
fn mac_signature(path: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let team = option_env!("DEVWORKS_SIGNING_TEAM").ok_or("The app has no pinned signing team.")?;
    let requirement = format!(
        "=anchor apple generic and identifier \"com.sceneworks.devworks\" and certificate leaf[subject.OU] = \"{team}\""
    );
    let mut child = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R", &requirement])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(native_error)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(native_error)? {
            return if status.success() {
                Ok(())
            } else {
                Err(
                    "The macOS application does not match the DevWorks platform signing identity."
                        .into(),
                )
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err("macOS application signature verification timed out.".into())
}

#[cfg(not(target_os = "macos"))]
fn mac_signature(_: &Path) -> Result<(), String> {
    Err("macOS installation is unavailable on this host.".into())
}

#[cfg(not(windows))]
fn windows_signature(_: &Path) -> Result<(), String> {
    Err("Windows installation is unavailable on this host.".into())
}

#[cfg(windows)]
pub(super) fn windows_signature(path: &Path) -> Result<(), String> {
    super::windows_trust::verify(path)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::fs;
    use std::fs::TryLockError;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn executable(root: &Path, contents: &str) -> PathBuf {
        let inside = root.join("devworks");
        fs::write(&inside, format!("#!/bin/sh\nprintf '%s' '{contents}'\n")).unwrap();
        fs::set_permissions(&inside, fs::Permissions::from_mode(0o700)).unwrap();
        inside
    }

    #[test]
    fn native_atomic_exchange_keeps_the_normal_entrypoint_through_both_boundaries() {
        let scratch = devworks_test_support::TempRoot::new("app-update-exchange").created();
        let current = scratch.join("installed");
        let prepared = scratch.join("predecessor");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&prepared).unwrap();
        let entry = executable(&current, "old");
        let previous_entry = executable(&prepared, "new");
        let run = |path: &Path| {
            let output = Command::new(path).output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        assert_eq!(run(&entry), b"old");
        assert_eq!(run(&previous_entry), b"new");
        swap(&current, &prepared).unwrap();
        sync_parents(&current, &prepared).unwrap();
        assert_eq!(run(&entry), b"new");
        assert_eq!(run(&previous_entry), b"old");
        swap(&current, &prepared).unwrap();
        sync_parents(&current, &prepared).unwrap();
        assert_eq!(run(&entry), b"old");
    }

    #[test]
    fn installation_gate_excludes_updater_while_another_profile_runs() {
        let scratch = devworks_test_support::TempRoot::new("app-update-ownership")
            .created()
            .respelled(|path| path.canonicalize().unwrap());
        let installed = scratch.join("installed");
        create_private_dir(&installed).unwrap();
        let owner = if cfg!(target_os = "macos") {
            InstallOwner::MacApplication
        } else {
            InstallOwner::LinuxStandalone
        };
        let install = Installation::at(owner, &installed).unwrap();
        let first = install.gate(false).unwrap();
        let second = install.gate(false).unwrap();
        drop(first);
        assert!(matches!(install.gate(true), Err(TryLockError::WouldBlock)));
        drop(second);
        let update = install.gate(true).unwrap();
        assert!(matches!(install.gate(false), Err(TryLockError::WouldBlock)));
        drop(update);
        assert!(install.gate(false).is_ok());
    }
}
