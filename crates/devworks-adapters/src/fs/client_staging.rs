//! Native UI staging custody. Scope comes from the stored session or typed editor owner.
use super::purge::capture_managed_file;
use devworks_core::ports::project_purge::{PurgeFileOwner, PurgeFileSelection};
use devworks_core::ports::storage::{
    ApplicationStore, ProjectManagedFileReservation, ReadOptions, StorageError,
    StoredProjectManagedFile, WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::{Id, ProjectId};

enum Owner {
    Session(Option<Id>),
    Project(Option<ProjectId>),
}
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct NativeClientStagingFiles {
    root: PathBuf,
    store: Arc<dyn ApplicationStore>,
}

impl NativeClientStagingFiles {
    pub fn new(root: PathBuf, store: Arc<dyn ApplicationStore>) -> Self {
        Self { root, store }
    }

    /// Only incomplete writes from a previous boot: no path was returned to a draft.
    /// Live paste files require the client keeper snapshot and are never swept here.
    pub fn recover_abandoned(&self) -> Result<(), String> {
        self.recover_kind("ClientPaste")?;
        self.recover_kind("ClientStaging")
    }

    fn recover_kind(&self, kind: &'static str) -> Result<(), String> {
        let boot = self.store.info().boot_id;
        let mut after = None;
        loop {
            let page = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |r| {
                r.projects()
                    .managed_file_recovery(kind, after.as_ref(), 500)
            })
            .map_err(|_| "staging recovery inventory is unavailable".to_owned())?;
            if page.is_empty() {
                return Ok(());
            }
            for row in &page {
                if !row.writing || row.boot_id == boot {
                    continue;
                }
                let info = self.store.info();
                if row.store_id != info.store_id || row.restore_epoch != info.restore_epoch {
                    return Err("staging recovery store or restore epoch changed".into());
                }
                if let Some(identity) = row.file_identity {
                    if super::normal_components(&row.relative_path).is_none_or(|p| p.len() != 1) {
                        return Err("staging recovery path is invalid".into());
                    }
                    let path = self.root.join(&row.relative_path);
                    match std::fs::symlink_metadata(path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                        Ok(_) => {
                            let root = open_root(&self.root).map_err(|e| e.to_string())?;
                            if super::identity_for_file(&root) != Some(row.root) {
                                return Err("staging recovery root changed".into());
                            }
                            let info = self.store.info();
                            let binding = PurgeFileSelection {
                                project_id: row.project_id.clone(),
                                store_id: info.store_id,
                                restore_epoch: info.restore_epoch,
                                owners: Vec::new(),
                            };
                            let witness = capture_managed_file(
                                &self.root,
                                &binding,
                                &row.relative_path,
                                file_owner(kind, &row.id),
                                &mut || false,
                            )
                            .map_err(|_| "staging recovery witness changed".to_owned())?;
                            if witness.root != row.root || witness.identity != identity {
                                return Err("staging recovery file changed".into());
                            }
                            let canonical_root =
                                std::fs::canonicalize(&self.root).map_err(|e| e.to_string())?;
                            super::purge::remove_native(&canonical_root, &witness, &mut || false)
                                .map_err(|_| "staging recovery removal is incomplete".to_owned())?;
                        }
                    }
                }
                // No bound identity means the producer could not yet write private bytes.
                // An unbound existing entry is independent and is never removed.
                let project = row.project_id.clone();
                let id = row.id.clone();
                run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                    tx.projects().finish_project_managed_file(&project, &id)
                })
                .map_err(|_| "staging recovery settlement is unavailable".to_owned())?;
            }
            after = page.last().map(|r| r.id.clone());
        }
    }

    /// Reserve before creation, bind the create-new identity before bytes, then publish.
    /// A failed project write stays durably Writing for the staging owner's recovery.
    pub fn write(&self, session: Option<&Id>, id: &Id, bytes: &[u8]) -> Result<PathBuf, String> {
        self.write_owned(
            Owner::Session(session.cloned()),
            id,
            "ClientPaste",
            "png",
            bytes,
        )
    }

    pub fn write_project(
        &self,
        project: Option<&ProjectId>,
        id: &Id,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        if !matches!(extension, "md" | "txt" | "json") {
            return Err("unsupported native staging format".into());
        }
        self.write_owned(
            Owner::Project(project.cloned()),
            id,
            "ClientStaging",
            extension,
            bytes,
        )
    }

    fn write_owned(
        &self,
        owner: Owner,
        id: &Id,
        kind: &'static str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.root).map_err(|e| e.to_string())?;
        let held_root = open_root(&self.root).map_err(|e| e.to_string())?;
        let root = super::identity_for_file(&held_root)
            .ok_or_else(|| "paste directory identity is unavailable".to_owned())?;
        let relative = format!("{}.{extension}", id.as_str());
        // Id's wire shape is not a filesystem capability.
        if super::normal_components(&relative).is_none_or(|parts| parts.len() != 1) {
            return Err("invalid staged-file identity".into());
        }
        #[cfg(windows)]
        if super::windows_unsafe_component(&relative) {
            return Err("invalid staged-file identity".into());
        }
        let reserved_id = id.clone();
        let reserved_path = relative.clone();
        let reservation = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let project = match owner {
                Owner::Project(project) => project,
                Owner::Session(None) => None,
                Owner::Session(Some(id)) => {
                    let s = tx
                        .sessions()
                        .session(&id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    if let Some(root) = &s.root_id {
                        let work = tx.work().work(&root.0)?.ok_or(StorageError::ScopeDenied)?;
                        if s.project_id.as_ref() != Some(&work.project_id) {
                            return Err(StorageError::ScopeDenied);
                        }
                    }
                    s.project_id
                }
            };
            let Some(project_id) = project else {
                return Ok(None);
            };
            let info = tx.store().clone();
            let file = ProjectManagedFileReservation {
                id: reserved_id,
                project_id,
                kind: kind.into(),
                relative_path: reserved_path,
                root,
                created_identity: None,
                store_id: info.store_id,
                restore_epoch: info.restore_epoch,
                boot_id: info.boot_id,
                created_at_ms: tx.now_ms(),
            };
            tx.projects().reserve_project_managed_file(&file)?;
            Ok(Some(file))
        })
        .map_err(|_| "the native owner cannot stage this file".to_owned())?;

        let mut file = match create(&held_root, &self.root, &relative) {
            Ok(file) => file,
            Err(error) => {
                if let Some(reserved) = reservation {
                    // This call never obtained a file: a colliding entry is independent.
                    run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                        tx.projects()
                            .finish_project_managed_file(&reserved.project_id, &reserved.id)
                    })
                    .map_err(|_| "staging reservation settlement is unavailable".to_owned())?;
                }
                return Err(error.to_string());
            }
        };
        let identity = super::identity_for_file(&file)
            .ok_or_else(|| "staged-file identity is unavailable".to_owned())?;
        if let Some(reserved) = &reservation {
            let project = reserved.project_id.clone();
            let id = id.clone();
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects()
                    .begin_project_managed_file_write(&project, &id, identity)
            })
            .map_err(|_| "staged-file creation could not be recorded".to_owned())?;
        }
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        #[cfg(unix)]
        held_root.sync_all().map_err(|e| e.to_string())?;

        if let Some(reserved) = reservation {
            let binding = PurgeFileSelection {
                project_id: reserved.project_id.clone(),
                store_id: reserved.store_id.clone(),
                restore_epoch: reserved.restore_epoch,
                owners: Vec::new(),
            };
            let witness = capture_managed_file(
                &self.root,
                &binding,
                &relative,
                file_owner(kind, id),
                &mut || false,
            )
            .map_err(|_| "staged-file publication witness changed".to_owned())?;
            if witness.root != root || witness.identity != identity {
                return Err("staged-file publication identity changed".into());
            }
            let published = StoredProjectManagedFile {
                id: id.clone(),
                project_id: reserved.project_id,
                kind: reserved.kind,
                relative_path: relative.clone(),
                witness,
                created_at_ms: reserved.created_at_ms,
            };
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().register_project_managed_file(&published)
            })
            .map_err(|_| "staged-file publication could not be recorded".to_owned())?;
        }
        Ok(self.root.join(relative))
    }
}

fn file_owner(kind: &str, id: &Id) -> PurgeFileOwner {
    match kind {
        "ClientPaste" => PurgeFileOwner::ClientPaste {
            file_id: id.clone(),
        },
        _ => PurgeFileOwner::ClientStaging {
            file_id: id.clone(),
        },
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
