//! Native UI staging directory: filesystem I/O only. Custody (reservation, the owner scope
//! check, publication and recovery) is `devworks_core::services::projects::managed_files`.
use super::purge::capture_managed_file;
use devworks_core::authority::destructive::FileIdentity;
use devworks_core::ports::managed_files::{StagingDirectory, StagingWrite};
use devworks_core::ports::project_purge::{
    PurgeFileFault, PurgeFileOwner, PurgeFileSelection, PurgeFileWitness,
};
use devworks_core::ports::storage::ProjectManagedFileRecovery;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct NativeClientStagingFiles {
    root: PathBuf,
}

impl NativeClientStagingFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl StagingDirectory for NativeClientStagingFiles {
    fn open(&self, name: &str) -> Result<Box<dyn StagingWrite + '_>, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let held_root = open_root(&self.root).map_err(|e| e.to_string())?;
        let root = super::identity_for_file(&held_root)
            .ok_or_else(|| "paste directory identity is unavailable".to_owned())?;
        // Id's wire shape is not a filesystem capability.
        if super::normal_components(name).is_none_or(|parts| parts.len() != 1) {
            return Err("invalid staged-file identity".into());
        }
        #[cfg(windows)]
        if super::windows_unsafe_component(name) {
            return Err("invalid staged-file identity".into());
        }
        Ok(Box::new(NativeStagingWrite {
            directory: self,
            held_root,
            root,
            name: name.to_owned(),
            file: None,
        }))
    }

    fn remove_abandoned(
        &self,
        row: &ProjectManagedFileRecovery,
        identity: FileIdentity,
        binding: &PurgeFileSelection,
        owner: PurgeFileOwner,
    ) -> Result<(), String> {
        if super::normal_components(&row.relative_path).is_none_or(|p| p.len() != 1) {
            return Err("staging recovery path is invalid".into());
        }
        let path = self.root.join(&row.relative_path);
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
            Ok(_) => {
                let root = open_root(&self.root).map_err(|e| e.to_string())?;
                if super::identity_for_file(&root) != Some(row.root) {
                    return Err("staging recovery root changed".into());
                }
                let witness = capture_managed_file(
                    &self.root,
                    binding,
                    &row.relative_path,
                    owner,
                    &mut || false,
                )
                .map_err(|_| "staging recovery witness changed".to_owned())?;
                if witness.root != row.root || witness.identity != identity {
                    return Err("staging recovery file changed".into());
                }
                let canonical_root =
                    std::fs::canonicalize(&self.root).map_err(|e| e.to_string())?;
                super::purge::remove_native(&canonical_root, &witness, &mut || false)
                    .map_err(|_| "staging recovery removal is incomplete".to_owned())
            }
        }
    }
}

struct NativeStagingWrite<'a> {
    directory: &'a NativeClientStagingFiles,
    held_root: File,
    root: FileIdentity,
    name: String,
    file: Option<File>,
}

impl StagingWrite for NativeStagingWrite<'_> {
    fn root(&self) -> FileIdentity {
        self.root
    }

    fn create(&mut self) -> Result<(), String> {
        let file =
            create(&self.held_root, &self.directory.root, &self.name).map_err(|e| e.to_string())?;
        self.file = Some(file);
        Ok(())
    }

    fn created(&self) -> Result<FileIdentity, String> {
        self.file
            .as_ref()
            .and_then(super::identity_for_file)
            .ok_or_else(|| "staged-file identity is unavailable".to_owned())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| "staged-file identity is unavailable".to_owned())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        #[cfg(unix)]
        self.held_root.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    }

    fn witness(
        &self,
        binding: &PurgeFileSelection,
        owner: PurgeFileOwner,
    ) -> Result<PurgeFileWitness, PurgeFileFault> {
        capture_managed_file(
            &self.directory.root,
            binding,
            &self.name,
            owner,
            &mut || false,
        )
    }

    fn path(&self) -> PathBuf {
        self.directory.root.join(&self.name)
    }
}

#[cfg(unix)]
fn open_root(root: &Path) -> std::io::Result<File> {
    if !std::fs::symlink_metadata(root)?.is_dir() {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    // Native temp/data paths may have platform aliases such as /var or /tmp.
    // Resolve those ancestors, then hold the exact root through a no-follow walk.
    Ok(super::unix::open_dir_nofollow(&std::fs::canonicalize(root)?)?.into())
}

#[cfg(windows)]
fn open_root(root: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::MetadataExt;
    let file = super::windows::open_held(root)?;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    Ok(file)
}

#[cfg(unix)]
fn create(root: &File, _path: &Path, name: &str) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name)?;
    // SAFETY: live held directory, one validated NUL-terminated component, mode supplied
    // for O_CREAT. O_EXCL refuses existing entries, including links.
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(windows)]
fn create(_root: &File, path: &Path, name: &str) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    if super::windows_unsafe_component(name) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path.join(name))
}

#[cfg(not(any(unix, windows)))]
fn open_root(_: &Path) -> std::io::Result<File> {
    Err(std::io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(unix, windows)))]
fn create(_: &File, _: &Path, _: &str) -> std::io::Result<File> {
    Err(std::io::ErrorKind::Unsupported.into())
}
