//! Verified file streams for the workspace archive. Unix and Windows path custody remains in
//! the existing verified root and platform owners.
use devworks_core::ports::storage::{Cancellation, FileDigest, StorageError, Unavailable};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub(crate) fn archive_io(error: std::io::Error) -> StorageError {
    let cause = match error.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            Unavailable::DiskFull
        }
        std::io::ErrorKind::PermissionDenied => Unavailable::ReadOnly,
        _ => Unavailable::Io,
    };
    StorageError::unavailable(cause, error.to_string())
}

fn transfer(
    mut source: impl Read,
    mut destination: impl Write,
    limit: u64,
    cancellation: &Cancellation,
) -> Result<FileDigest, StorageError> {
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut bytes = 0_u64;
    let mut digest = Sha256::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let len = source.read(&mut buffer).map_err(archive_io)?;
        if len == 0 {
            break;
        }
        bytes = bytes
            .checked_add(len as u64)
            .filter(|total| *total <= limit)
            .ok_or_else(|| {
                StorageError::InvalidRecord("workspace member is larger than recorded".into())
            })?;
        digest.update(&buffer[..len]);
        destination.write_all(&buffer[..len]).map_err(archive_io)?;
    }
    Ok(FileDigest {
        sha256: digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        bytes,
    })
}

/// Hash a regular file in the caller's owned private stage. Source files outside that state
/// must be opened through the native verified-reader root before entering the copy operation.
pub(crate) fn digest_private_file(
    path: &Path,
    limit: u64,
    cancellation: &Cancellation,
) -> Result<FileDigest, StorageError> {
    if !std::fs::symlink_metadata(path)
        .map_err(archive_io)?
        .is_file()
    {
        return Err(StorageError::InvalidRecord(
            "a workspace member is not a regular file".into(),
        ));
    }
    transfer(
        File::open(path).map_err(archive_io)?,
        std::io::sink(),
        limit,
        cancellation,
    )
}

/// Write a fresh private member and seal it against the exact expected content.
pub(crate) fn copy_verified_member(
    source: impl Read,
    destination: &Path,
    expected: &FileDigest,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(archive_io)?;
    let result = transfer(source, &mut file, expected.bytes, cancellation).and_then(|actual| {
        if &actual != expected {
            return Err(StorageError::InvalidRecord(
                "a workspace member does not match its digest".into(),
            ));
        }
        file.sync_all().map_err(archive_io)
    });
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

use super::NativeFilesystem;
use devworks_core::authority::destructive::FileIdentity;
use devworks_core::authority::reads::{
    ReadRefusal, ReadRequest, ReadRoot, VerifiedFile, VerifiedReader,
};
use devworks_core::ports::storage::{BlobWitness, PackageWitness};
use devworks_core::protocol::Id;
use std::path::PathBuf;

/// A verified directory opened by native custody. The path and stable identity are recorded in
/// an owned permit and checked again on retries. Children never accept unchecked path components.
pub(crate) struct ArchiveDirectory {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
}

impl ArchiveDirectory {
    pub(crate) fn open(path: &Path, expected: Option<FileIdentity>) -> Result<Self, StorageError> {
        let handle = super::client_staging::open_root(path).map_err(archive_io)?;
        let identity = super::identity_for_file(&handle).ok_or(StorageError::ScopeDenied)?;
        if expected.is_some_and(|value| value != identity) {
            return Err(StorageError::ScopeDenied);
        }
        let canonical = std::fs::canonicalize(path).map_err(archive_io)?;
        let proof = super::client_staging::open_root(&canonical).map_err(archive_io)?;
        if super::identity_for_file(&proof) != Some(identity) {
            return Err(StorageError::ScopeDenied);
        }
        Ok(Self {
            path: canonical,
            handle,
            identity,
        })
    }

    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn location(&self) -> &Path {
        &self.path
    }

    pub(crate) fn create_folder(&self, name: &str) -> Result<Self, StorageError> {
        member(name)?;
        create_directory(&self.handle, &self.path, name).map_err(archive_io)?;
        self.flush()?;
        self.child(name)
    }

    pub(crate) fn child(&self, name: &str) -> Result<Self, StorageError> {
        member(name)?;
        let child = open_child(&self.handle, &self.path, name).map_err(archive_io)?;
        let identity = super::identity_for_file(&child).ok_or(StorageError::ScopeDenied)?;
        Ok(Self {
            path: self.path.join(name),
            handle: child,
            identity,
        })
    }

    pub(crate) fn open_member(
        &self,
        name: &str,
        max_bytes: u64,
    ) -> Result<VerifiedFile, StorageError> {
        member(name)?;
        let root = ReadRoot {
            location: self.path.clone(),
            identity: Some(self.identity),
        };
        NativeFilesystem::new()
            .open(&ReadRequest {
                root: &root,
                relative: name,
                max_bytes,
                allow_hard_links: false,
            })
            .map_err(|error| match error {
                ReadRefusal::NotFound => StorageError::NotFound {
                    kind: "backup-member",
                    id: name.to_owned(),
                },
                _ => StorageError::InvalidRecord(
                    "the workspace archive member could not be verified".into(),
                ),
            })
    }

    // If a concurrent change invalidates a directory identity, neither read nor retry can treat
    // these bytes as published. Unix creation addresses the already-held directory descriptor.
    pub(crate) fn write_member(
        &self,
        name: &str,
        source: impl Read,
        expected: &FileDigest,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        member(name)?;
        let mut file =
            super::client_staging::create(&self.handle, &self.path, name).map_err(archive_io)?;
        let digest = transfer(source, &mut file, expected.bytes, cancellation)?;
        if &digest != expected {
            return Err(StorageError::InvalidRecord(
                "a workspace archive member changed during copy".into(),
            ));
        }
        file.sync_all().map_err(archive_io)?;
        self.flush()
    }

    pub(crate) fn publish_child(&self, source: &Self, name: &str) -> Result<(), StorageError> {
        member(name)?;
        let sibling = source
            .location()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or(StorageError::ScopeDenied)?;
        if self.child(sibling)?.identity() != source.identity() {
            return Err(StorageError::ScopeDenied);
        }
        rename_new(&self.handle, &self.path, sibling, name).map_err(archive_io)?;
        self.flush().map_err(|_| StorageError::CommitUnknown)
    }

    fn write_staged_member(
        &self,
        name: &str,
        source: impl Read,
        expected: &FileDigest,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        match self.open_member(name, expected.bytes) {
            Ok(known) => {
                if transfer(known.reader, std::io::sink(), expected.bytes, cancellation)?
                    == *expected
                {
                    return Ok(());
                }
                return Err(StorageError::InvalidRecord(
                    "the staged workspace member changed".into(),
                ));
            }
            Err(StorageError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        let pending = format!("stage-{}.pending", transient_id()?);
        self.write_member(&pending, source, expected, cancellation)?;
        rename_new(&self.handle, &self.path, &pending, name).map_err(archive_io)?;
        self.flush()
    }

    pub(crate) fn seal_record(
        &self,
        pending: &str,
        name: &str,
        bytes: &[u8],
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        member(pending)?;
        member(name)?;
        let expected = FileDigest {
            sha256: devworks_core::fingerprint::sha256_hex(bytes),
            bytes: bytes.len() as u64,
        };
        self.write_member(pending, bytes, &expected, cancellation)?;
        rename_new(&self.handle, &self.path, pending, name).map_err(archive_io)?;
        self.flush().map_err(|_| StorageError::CommitUnknown)
    }

    pub(crate) fn publish_database(
        &self,
        candidate: &ArchiveDirectory,
        name: &str,
    ) -> Result<(), StorageError> {
        member(name)?;
        replace_member(candidate, self, name).map_err(archive_io)?;
        self.flush().map_err(|_| StorageError::CommitUnknown)?;
        candidate.flush().map_err(|_| StorageError::CommitUnknown)
    }

    /// Seal by renaming a complete, synced manifest without replacing an existing name.
    pub(crate) fn publish_manifest(
        &self,
        bytes: &[u8],
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        use devworks_core::ports::storage::MANIFEST_MEMBER;
        const PENDING: &str = "manifest.pending";
        let expected = FileDigest {
            sha256: devworks_core::fingerprint::sha256_hex(bytes),
            bytes: bytes.len() as u64,
        };
        self.write_member(PENDING, bytes, &expected, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        // The manifest is the publication boundary. A failed directory flush after rename is
        // ambiguous and the journal worker must reopen and compare the exact complete archive.
        rename_new(&self.handle, &self.path, PENDING, MANIFEST_MEMBER).map_err(archive_io)?;
        self.flush().map_err(|_| StorageError::CommitUnknown)
    }

    pub(crate) fn flush(&self) -> Result<(), StorageError> {
        #[cfg(unix)]
        self.handle.sync_all().map_err(archive_io)?;
        Ok(())
    }
}

fn transient_id() -> Result<String, StorageError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        StorageError::unavailable(Unavailable::Io, "cannot create an isolated archive stage")
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn member(name: &str) -> Result<(), StorageError> {
    if name.is_empty()
        || name.len() > 240
        || name.starts_with('.')
        || name.ends_with('.')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(StorageError::ScopeDenied);
    }
    #[cfg(windows)]
    if super::windows_unsafe_component(name) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(crate) fn blob_member(blob: &BlobWitness) -> String {
    format!("blob-{}.{}", blob.content.sha256, blob.generation.as_str())
}

pub(crate) fn package_member(package: &PackageWitness, path: &str) -> String {
    // The hashed name is derived from the signed package identity and manifest-relative path.
    // Archive readers do not treat that relative path as a directory to open in the archive.
    let identity = serde_json::json!([package.package, package.manifest_digest, path]);
    format!(
        "package-{}",
        devworks_core::fingerprint::sha256_hex(identity.to_string().as_bytes())
    )
}

#[cfg(unix)]
fn create_directory(root: &File, _: &Path, name: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name)?;
    // SAFETY: the name is one checked component relative to a live directory descriptor.
    if unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
#[cfg(unix)]
fn open_child(root: &File, _: &Path, name: &str) -> std::io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name)?;
    super::unix::open_at(
        root.as_raw_fd(),
        &name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
    .map(File::from)
}
#[cfg(windows)]
fn create_directory(_: &File, path: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir(path.join(name))
}
#[cfg(windows)]
fn open_child(_: &File, path: &Path, name: &str) -> std::io::Result<File> {
    super::client_staging::open_root(&path.join(name))
}

#[cfg(target_os = "macos")]
fn rename_new(root: &File, _: &Path, from: &str, to: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (from, to) = (std::ffi::CString::new(from)?, std::ffi::CString::new(to)?);
    let root = root.as_raw_fd();
    // SAFETY: both names are checked siblings and the descriptor stays live.
    if unsafe { libc::renameatx_np(root, from.as_ptr(), root, to.as_ptr(), libc::RENAME_EXCL) } == 0
    {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
#[cfg(target_os = "linux")]
fn rename_new(root: &File, _: &Path, from: &str, to: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let (from, to) = (std::ffi::CString::new(from)?, std::ffi::CString::new(to)?);
    let root = root.as_raw_fd();
    // SAFETY: both names are checked siblings and the descriptor stays live.
    if unsafe {
        libc::renameat2(
            root,
            from.as_ptr(),
            root,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
#[cfg(windows)]
fn rename_new(_: &File, path: &Path, from: &str, to: &str) -> std::io::Result<()> {
    super::artifacts::durable::rename(&path.join(from), &path.join(to), false)
}
#[cfg(not(any(unix, windows)))]
fn create_directory(_: &File, _: &Path, _: &str) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(unix, windows)))]
fn open_child(_: &File, _: &Path, _: &str) -> std::io::Result<File> {
    Err(std::io::ErrorKind::Unsupported.into())
}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_new(_: &File, _: &Path, _: &str, _: &str) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(unix)]
fn replace_member(
    source: &ArchiveDirectory,
    destination: &ArchiveDirectory,
    name: &str,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name)?;
    // SAFETY: both descriptors remain open and the checked name is a single component.
    if unsafe {
        libc::renameat(
            source.handle.as_raw_fd(),
            name.as_ptr(),
            destination.handle.as_raw_fd(),
            name.as_ptr(),
        )
    } == 0
    {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
#[cfg(windows)]
fn replace_member(
    source: &ArchiveDirectory,
    destination: &ArchiveDirectory,
    name: &str,
) -> std::io::Result<()> {
    super::artifacts::durable::rename(
        &source.location().join(name),
        &destination.location().join(name),
        true,
    )
}
#[cfg(not(any(unix, windows)))]
fn replace_member(_: &ArchiveDirectory, _: &ArchiveDirectory, _: &str) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

use devworks_core::ports::files::ArtifactFiles;
use devworks_core::ports::packages::PackageFiles;
use devworks_core::ports::storage::{
    ARCHIVE_FORMAT, ARCHIVE_REVISION, DATABASE_MEMBER, MANIFEST_MEMBER, WorkspaceArchive,
    WorkspaceCheckpoint,
};
use devworks_core::ports::workspace_archive::{BackupFolder, WorkspaceArchiveFiles};
use std::sync::Arc;

/// Typed access to native backup directories and the existing immutable file owners.
pub struct NativeWorkspaceArchives {
    data_root: PathBuf,
    artifacts: Arc<dyn ArtifactFiles>,
    packages: Arc<dyn PackageFiles>,
}

impl NativeWorkspaceArchives {
    pub fn new(
        data_root: PathBuf,
        artifacts: Arc<dyn ArtifactFiles>,
        packages: Arc<dyn PackageFiles>,
    ) -> Self {
        Self {
            data_root,
            artifacts,
            packages,
        }
    }

    fn stages(&self) -> Result<ArchiveDirectory, StorageError> {
        let root = ArchiveDirectory::open(&self.data_root, None)?;
        root.child("workspace-archives").or_else(|_| {
            root.create_folder("workspace-archives")
                .or_else(|_| root.child("workspace-archives"))
        })
    }

    pub(crate) fn stage(&self, id: &Id) -> Result<ArchiveDirectory, StorageError> {
        self.stages()?.child(id.as_str())
    }

    /// During stopped-store recovery, the OS data-root lock excludes the normal file owners.
    /// Reuse exact generations only after verifying their bytes, and never repair a conflicting
    /// destination in place.
    pub(crate) fn install_members(
        &self,
        id: &Id,
        archive: &WorkspaceArchive,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        use devworks_core::ports::files::FileFault;
        use devworks_core::ports::packages::{BundleFile, InspectedFile, MAX_BUNDLE_BYTES};

        let stage = self.stage(id)?;
        verify_members(&stage, archive, cancellation)?;
        for blob in &archive.checkpoint.blobs {
            let digest = &blob.content.sha256;
            let generation = blob.generation.as_str();
            match self.artifacts.open_controlled(
                digest,
                generation,
                blob.content.bytes,
                &mut || cancellation.is_cancelled(),
            ) {
                Ok(_) => continue,
                Err(FileFault::NotFound) => {}
                Err(_) => {
                    return Err(StorageError::InvalidRecord(
                        "existing artifact generation conflicts with the restore".into(),
                    ));
                }
            }
            let mut source = stage.open_member(&blob_member(blob), blob.content.bytes)?;
            let staging_name = format!("restore-{generation}");
            self.artifacts
                .publish(
                    &staging_name,
                    &mut source.reader,
                    digest,
                    blob.content.bytes,
                    None,
                    generation,
                )
                .map_err(|_| {
                    StorageError::InvalidRecord(
                        "restored artifact generation could not enter custody".into(),
                    )
                })?;
        }
        for package in &archive.checkpoint.packages {
            let package_id = package.package.as_str();
            if !package_id
                .bytes()
                .all(|x| x.is_ascii_alphanumeric() || matches!(x, b'_' | b'-'))
            {
                return Err(StorageError::ScopeDenied);
            }
            let digest = &package.manifest_digest;
            let directory = self.packages.directory(package_id, digest);
            let expected = package
                .files
                .iter()
                .map(|file| InspectedFile {
                    path: file.path.clone(),
                    sha256: file.content.sha256.clone(),
                    byte_length: file.content.bytes,
                    executable: file.executable,
                })
                .collect::<Vec<_>>();
            match std::fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.is_dir() => {
                    self.packages
                        .verify(package_id, digest, &expected)
                        .map_err(|_| {
                            StorageError::InvalidRecord(
                                "existing package version conflicts with the restore".into(),
                            )
                        })?;
                    continue;
                }
                Ok(_) => return Err(StorageError::ScopeDenied),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(archive_io(error)),
            }
            // The package owner already has a bounded memory-backed publish interface.
            let mut remaining = MAX_BUNDLE_BYTES;
            let mut files = Vec::with_capacity(expected.len());
            for file in &package.files {
                remaining = remaining.checked_sub(file.content.bytes).ok_or_else(|| {
                    StorageError::InvalidRecord("restored package exceeds supported size".into())
                })?;
                let opened =
                    stage.open_member(&package_member(package, &file.path), file.content.bytes)?;
                let mut bytes = Vec::new();
                let actual = transfer(opened.reader, &mut bytes, file.content.bytes, cancellation)?;
                if actual != file.content {
                    return Err(StorageError::InvalidRecord(
                        "staged package file changed".into(),
                    ));
                }
                files.push(BundleFile {
                    path: file.path.clone(),
                    bytes,
                    executable: file.executable,
                });
            }
            self.packages
                .publish(package_id, digest, &files, &expected)
                .map_err(|_| {
                    StorageError::InvalidRecord(
                        "restored package version could not enter custody".into(),
                    )
                })?;
        }
        Ok(())
    }

    fn external(&self, root: &BackupFolder) -> Result<ArchiveDirectory, StorageError> {
        let folder = ArchiveDirectory::open(&root.path, Some(root.identity))?;
        if folder
            .location()
            .starts_with(std::fs::canonicalize(&self.data_root).map_err(archive_io)?)
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(folder)
    }
}

/// All member names are derived. No path from an archive is interpreted relative to the app's
/// live profile. Each length/hash is checked again when a member enters private custody.
fn archive_members(
    archive: &WorkspaceArchive,
    mut visit: impl FnMut(&str, &FileDigest) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    visit(DATABASE_MEMBER, &archive.database)?;
    for blob in &archive.checkpoint.blobs {
        visit(&blob_member(blob), &blob.content)?;
    }
    for package in &archive.checkpoint.packages {
        for file in &package.files {
            visit(&package_member(package, &file.path), &file.content)?;
        }
    }
    Ok(())
}

const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;

fn read_manifest(directory: &ArchiveDirectory) -> Result<WorkspaceArchive, StorageError> {
    let mut file = directory.open_member(MANIFEST_MEMBER, MAX_MANIFEST_BYTES)?;
    let mut bytes = Vec::new();
    file.reader.read_to_end(&mut bytes).map_err(archive_io)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(StorageError::InvalidRecord(
            "workspace archive manifest is too large".into(),
        ));
    }
    let manifest: WorkspaceArchive = serde_json::from_slice(&bytes)
        .map_err(|_| StorageError::InvalidRecord("invalid workspace archive manifest".into()))?;
    manifest.validate()?;
    Ok(manifest)
}

fn seal(
    directory: &ArchiveDirectory,
    manifest: &WorkspaceArchive,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| {
        StorageError::InvalidRecord("workspace manifest could not be serialized".into())
    })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(StorageError::InvalidRecord(
            "workspace archive manifest is too large".into(),
        ));
    }
    directory.publish_manifest(&bytes, cancellation)
}

fn copy_members(
    source: &ArchiveDirectory,
    destination: &ArchiveDirectory,
    manifest: &WorkspaceArchive,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    archive_members(manifest, |name, digest| {
        let file = source.open_member(name, digest.bytes)?;
        destination.write_member(name, file.reader, digest, cancellation)
    })
}

fn verify_members(
    directory: &ArchiveDirectory,
    manifest: &WorkspaceArchive,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    archive_members(manifest, |name, digest| {
        let member = directory.open_member(name, digest.bytes)?;
        if transfer(member.reader, std::io::sink(), digest.bytes, cancellation)? != *digest {
            return Err(StorageError::InvalidRecord(
                "a workspace archive member failed verification".into(),
            ));
        }
        Ok(())
    })
}

fn export_name(id: &Id) -> String {
    format!("devworks-backup-{}", id.as_str())
}

impl WorkspaceArchiveFiles for NativeWorkspaceArchives {
    fn bind_external_folder(&self, path: &Path) -> Result<BackupFolder, StorageError> {
        let opened = ArchiveDirectory::open(path, None)?;
        let bound = BackupFolder {
            path: opened.location().to_path_buf(),
            identity: opened.identity(),
        };
        self.external(&bound)?;
        Ok(bound)
    }

    fn create_stage(&self, id: &Id) -> Result<PathBuf, StorageError> {
        Ok(self
            .stages()?
            .create_folder(id.as_str())?
            .location()
            .join(DATABASE_MEMBER))
    }

    fn discard_stage(&self, id: &Id) -> Result<(), StorageError> {
        let stage = self.stage(id)?;
        let parent = self.stages()?;
        let path = stage.location().to_path_buf();
        let identity = stage.identity();
        drop(stage);
        if parent.child(id.as_str())?.identity() != identity {
            return Err(StorageError::ScopeDenied);
        }
        std::fs::remove_dir_all(path).map_err(archive_io)?;
        parent.flush()
    }

    fn staged_database(&self, id: &Id) -> Result<PathBuf, StorageError> {
        Ok(self.stage(id)?.location().join(DATABASE_MEMBER))
    }

    fn stage_packages(
        &self,
        id: &Id,
        checkpoint: &WorkspaceCheckpoint,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        let stage = self.stage(id)?;
        let reader = NativeFilesystem::new();
        for package in &checkpoint.packages {
            if !devworks_core::fingerprint::is_sha256_hex(&package.manifest_digest) {
                return Err(StorageError::InvalidRecord(
                    "invalid package digest in checkpoint".into(),
                ));
            }
            let path = self
                .packages
                .directory(package.package.as_str(), &package.manifest_digest);
            let directory = ArchiveDirectory::open(&path, None)?;
            let root = ReadRoot {
                location: directory.location().to_path_buf(),
                identity: Some(directory.identity()),
            };
            for item in &package.files {
                // The snapshot's authoritative package inventory comes from the package service's
                // verified install record. The existing no-follow reader and streamed digest
                // revalidate each file once while the package owner holds collection.
                let file = reader
                    .open(&ReadRequest {
                        root: &root,
                        relative: &item.path,
                        max_bytes: item.content.bytes,
                        allow_hard_links: false,
                    })
                    .map_err(|_| {
                        StorageError::InvalidRecord(
                            "a captured installed package file could not be verified".into(),
                        )
                    })?;
                stage.write_staged_member(
                    &package_member(package, &item.path),
                    file.reader,
                    &item.content,
                    cancellation,
                )?;
            }
        }
        Ok(())
    }

    fn stage_blobs(
        &self,
        id: &Id,
        checkpoint: &WorkspaceCheckpoint,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        let stage = self.stage(id)?;
        for blob in &checkpoint.blobs {
            let reader = self
                .artifacts
                .open_controlled(
                    &blob.content.sha256,
                    blob.generation.as_str(),
                    blob.content.bytes,
                    &mut || cancellation.is_cancelled(),
                )
                .map_err(|_| {
                    if cancellation.is_cancelled() {
                        StorageError::Cancelled
                    } else {
                        StorageError::InvalidRecord(
                            "a captured managed artifact could not be verified".into(),
                        )
                    }
                })?;
            stage.write_staged_member(&blob_member(blob), reader, &blob.content, cancellation)?;
        }
        Ok(())
    }

    fn seal_stage(
        &self,
        id: &Id,
        checkpoint: WorkspaceCheckpoint,
        app_build: &str,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceArchive, StorageError> {
        let archive = WorkspaceArchive {
            format: ARCHIVE_FORMAT.into(),
            revision: ARCHIVE_REVISION,
            operation_id: id.clone(),
            app_build: app_build.to_owned(),
            checkpoint,
            database: self.staged_database_digest(id, cancellation)?,
        };
        archive.validate()?;
        let stage = self.stage(id)?;
        verify_members(&stage, &archive, cancellation)?;
        seal(&stage, &archive, cancellation)?;
        Ok(archive)
    }

    fn stage_external_archive(
        &self,
        folder: &BackupFolder,
        id: &Id,
        cancellation: &Cancellation,
    ) -> Result<WorkspaceArchive, StorageError> {
        let source = self.external(folder)?;
        let archive = read_manifest(&source)?;
        let staged = self.stages()?.create_folder(id.as_str())?;
        copy_members(&source, &staged, &archive, cancellation)?;
        seal(&staged, &archive, cancellation)?;
        Ok(archive)
    }

    fn read_stage(&self, id: &Id) -> Result<WorkspaceArchive, StorageError> {
        read_manifest(&self.stage(id)?)
    }

    fn staged_database_digest(
        &self,
        id: &Id,
        cancellation: &Cancellation,
    ) -> Result<FileDigest, StorageError> {
        let file = self.stage(id)?.open_member(DATABASE_MEMBER, u64::MAX)?;
        transfer(file.reader, std::io::sink(), u64::MAX, cancellation)
    }

    fn observe_publication(
        &self,
        destination: &BackupFolder,
        id: &Id,
        manifest: &WorkspaceArchive,
        cancellation: &Cancellation,
    ) -> Result<bool, StorageError> {
        let root = self.external(destination)?;
        let final_path = root.location().join(export_name(id));
        match std::fs::symlink_metadata(final_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(archive_io(error)),
            Ok(_) => {}
        }
        let published = root.child(&export_name(id))?;
        if read_manifest(&published)? != *manifest {
            return Err(StorageError::InvalidRecord(
                "the destination holds a different archive".into(),
            ));
        }
        verify_members(&published, manifest, cancellation)?;
        Ok(true)
    }

    fn publish(
        &self,
        destination: &BackupFolder,
        id: &Id,
        manifest: &WorkspaceArchive,
        cancellation: &Cancellation,
        permit: &mut dyn FnMut() -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        if self.observe_publication(destination, id, manifest, cancellation)? {
            return Ok(());
        }
        let root = self.external(destination)?;
        let stage_name = format!("archive-{}.pending", transient_id()?);
        let archive = root.create_folder(&stage_name)?;
        let copied = (|| {
            copy_members(&self.stage(id)?, &archive, manifest, cancellation)?;
            seal(&archive, manifest, cancellation)?;
            permit()?;
            root.publish_child(&archive, &export_name(id))
        })();
        copied?;
        if self.observe_publication(destination, id, manifest, cancellation)? {
            return Ok(());
        }
        Err(StorageError::CommitUnknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::storage::MANIFEST_MEMBER;

    #[test]
    fn publication_uses_the_held_directory_and_never_replaces_a_manifest() {
        let root = devworks_test_support::TempRoot::new("devworks-archive-file").created();
        let directory = ArchiveDirectory::open(root.path(), None).unwrap();
        let backup = directory.create_folder("backup-test").unwrap();
        let cancel = Cancellation::default();
        let data = b"first manifest";
        backup.publish_manifest(data, &cancel).unwrap();
        let mut actual = Vec::new();
        backup
            .open_member(MANIFEST_MEMBER, 64)
            .unwrap()
            .reader
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, data);
        assert!(backup.publish_manifest(b"unrelated", &cancel).is_err());
        assert_eq!(
            std::fs::read(backup.location().join(MANIFEST_MEMBER)).unwrap(),
            data
        );
        assert!(directory.create_folder("backup-test").is_err());
        assert!(ArchiveDirectory::open(backup.location(), Some(directory.identity())).is_err());
        assert!(backup.open_member("../manifest.json", 64).is_err());
    }
}
