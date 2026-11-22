//! This machine's own folder under a root that several machines may share:
//! `<root>/hosts/<host>/`.
//!
//! Everything one host writes and decides about alone (locks, build slots, temp folders,
//! campaign work, the runner folder, Docker) lives there; `weights/`, `cache/` and campaign
//! evidence stay shared at the root. `<host>` is the name the operating system reports, never
//! a variable a caller could override:
//! - macOS: the LocalHostName (`scutil --get LocalHostName`);
//! - Windows: the computer's NetBIOS name (`GetComputerNameExW`, the value `COMPUTERNAME`
//!   carries);
//! - other Unix: `gethostname`.
//!
//! The name is reduced to `[A-Za-z0-9._-]`. Only `dev-host init-host` creates the folder.

use std::path::{Path, PathBuf};

use crate::{ROOT_VAR, is_under};

/// The folder under the root that holds one folder per host.
pub const HOSTS_DIR: &str = "hosts";
/// The lock folder inside a host's folder.
pub const LOCKS_DIR: &str = "locks";

/// A verified root and the name of this host's folder in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    root: PathBuf,
    name: String,
}

impl Host {
    /// Pairs `root` with a host name, reduced by [`sanitize`].
    pub fn new(root: PathBuf, name: &str) -> Result<Self, String> {
        Ok(Self {
            root,
            name: sanitize(name)?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `<root>/hosts/<host>`.
    pub fn dir(&self) -> PathBuf {
        self.root.join(HOSTS_DIR).join(&self.name)
    }

    /// `<root>/hosts/<host>/locks`.
    pub fn locks(&self) -> PathBuf {
        self.dir().join(LOCKS_DIR)
    }

    /// Accepts a caller-chosen build folder (`CARGO_TARGET_DIR`, `--target-dir`) only inside this
    /// host's folder: anywhere else on a shared root (another host's folder, or a top-level
    /// folder such as a 0.1 `build/`) another host may build into it too, and cleanup never
    /// reclaims it.
    /// `what` names the setting in the error. Returns the resolved path.
    pub fn allow_build_dir(&self, what: &str, path: &Path, cwd: &Path) -> Result<PathBuf, String> {
        if !is_under(path, &self.root, cwd) {
            return Err(format!(
                "{what} {} is outside {ROOT_VAR}={}; drop it so dev-host assigns a slot, or \
                 point it under the root",
                path.display(),
                self.root.display()
            ));
        }
        if !is_under(path, &self.dir(), cwd) {
            return Err(format!(
                "{what} {} is not inside this host's folder {}, and another host may be \
                 using it; drop it so dev-host assigns a slot",
                path.display(),
                self.dir().display()
            ));
        }
        Ok(crate::resolve(path, cwd))
    }
}

/// Keeps only `[A-Za-z0-9._-]` of a host name. Refuses a name with nothing left, or with only
/// dots left (`.` and `..` are not folder names).
pub fn sanitize(raw: &str) -> Result<String, String> {
    let name: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if name.chars().all(|c| c == '.') {
        return Err(format!(
            "the host name the operating system reports ({raw:?}) has no letters or digits \
             usable as a folder name; give this machine a local host name"
        ));
    }
    Ok(name)
}

/// This machine's host name as the operating system reports it (not yet sanitized).
#[cfg(target_os = "macos")]
pub fn local_name() -> Result<String, String> {
    // By absolute path, so a `scutil` earlier on PATH cannot choose the name.
    let out = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .output()
        .map_err(|e| format!("cannot run scutil to read the local host name: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "scutil --get LocalHostName failed ({}); set the local host name in System \
             Settings > General > Sharing",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// This machine's host name as the operating system reports it (not yet sanitized).
#[cfg(all(unix, not(target_os = "macos")))]
pub fn local_name() -> Result<String, String> {
    unsafe extern "C" {
        fn gethostname(name: *mut std::ffi::c_char, len: usize) -> std::ffi::c_int;
    }
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its full length, which is passed as `len`.
    let rc = unsafe { gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return Err(format!(
            "gethostname failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(String::from_utf8_lossy(&buf[..end]).trim().to_owned())
}

/// This machine's host name as the operating system reports it (not yet sanitized).
#[cfg(windows)]
pub fn local_name() -> Result<String, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetComputerNameExW(name_type: i32, buffer: *mut u16, size: *mut u32) -> i32;
    }
    // sysinfoapi.h: COMPUTER_NAME_FORMAT::ComputerNamePhysicalNetBIOS, this machine's own
    // NetBIOS name (the value COMPUTERNAME carries) even on a cluster node.
    const COMPUTER_NAME_PHYSICAL_NETBIOS: i32 = 4;
    let mut size = 0u32;
    // SAFETY: a null buffer with size 0 only asks for the required size.
    unsafe {
        GetComputerNameExW(
            COMPUTER_NAME_PHYSICAL_NETBIOS,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    let mut buf = vec![0u16; size.max(1) as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is writable for `len` UTF-16 units.
    let ok =
        unsafe { GetComputerNameExW(COMPUTER_NAME_PHYSICAL_NETBIOS, buf.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return Err(format!(
            "GetComputerNameExW failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_safe_characters_and_refuses_nothing_left() {
        assert_eq!(
            sanitize("Studio-Mac.local_2").as_deref(),
            Ok("Studio-Mac.local_2")
        );
        assert_eq!(sanitize("Mäc mini/x").as_deref(), Ok("Mcminix"));
        for bad in ["", "  ", ".", "..", "/\\", "ä"] {
            assert!(sanitize(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn host_folders_sit_under_hosts() {
        let host = Host::new(PathBuf::from("/r"), "mac-a").unwrap();
        assert_eq!(host.dir(), Path::new("/r").join("hosts").join("mac-a"));
        assert_eq!(host.locks(), host.dir().join("locks"));
        assert!(Host::new(PathBuf::from("/r"), "..").is_err());
    }

    #[test]
    fn local_name_is_usable() {
        // Every CI and dev machine has a host name; it must survive sanitizing.
        let raw = local_name().unwrap();
        assert!(sanitize(&raw).is_ok(), "{raw:?}");
    }

    #[test]
    fn caller_build_dirs_stay_out_of_other_hosts() {
        let scratch = crate::testutil::Scratch::new("host-allow");
        let root = scratch.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let host = Host::new(root.clone(), "mac-a").unwrap();
        let cwd = scratch.path();
        assert!(
            host.allow_build_dir("X", &host.dir().join("build/c"), cwd)
                .is_ok()
        );
        for shared in [
            root.join("hosts/mac-b/build/c"),
            root.join("build/legacy"),
            root.clone(),
        ] {
            let err = host.allow_build_dir("X", &shared, cwd).unwrap_err();
            assert!(err.contains("another host"), "{}: {err}", shared.display());
        }
        let outside = host
            .allow_build_dir("X", &scratch.path().join("target"), cwd)
            .unwrap_err();
        assert!(outside.contains("outside DEV_HOST_ROOT"), "{outside}");
    }
}
