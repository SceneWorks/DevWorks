//! Native managed-file witnesses shared by staging producers and reviewed purge inventory.
use super::{NativeFilesystem, identity_at};
use devworks_core::authority::reads::{ReadRefusal, ReadRequest, ReadRoot, VerifiedReader};
use devworks_core::ports::project_purge::{PurgeFileFault, PurgeFileOwner, PurgeFileWitness};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

fn read_fault(error: ReadRefusal) -> PurgeFileFault {
    match error {
        ReadRefusal::Unsupported(_) => PurgeFileFault::Unsupported,
        ReadRefusal::Io(_) => PurgeFileFault::Io,
        ReadRefusal::HardLinked => PurgeFileFault::Unattributed,
        _ => PurgeFileFault::Changed,
    }
}

fn joined_relative(root: &Path, relative: &str) -> Result<std::path::PathBuf, PurgeFileFault> {
    let parts = super::normal_components(relative).ok_or(PurgeFileFault::Changed)?;
    let mut path = root.to_owned();
    // Push components separately: Windows verbatim roots do not normalize '/'.
    for part in parts {
        path.push(part);
    }
    Ok(path)
}

/// Capture a normal file through the existing no-follow, identity-checked reader.
/// This accepts a native managed root, never a wire-supplied filesystem location.
pub(crate) fn capture_managed_file(
    root: &Path,
    binding: &devworks_core::ports::project_purge::PurgeFileSelection,
    relative: &str,
    owner: PurgeFileOwner,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<PurgeFileWitness, PurgeFileFault> {
    if stopped() {
        return Err(PurgeFileFault::Cancelled);
    }
    let metadata = std::fs::symlink_metadata(root).map_err(|_| PurgeFileFault::Io)?;
    if !metadata.is_dir() {
        return Err(PurgeFileFault::Changed);
    }
    let root_identity = identity_at(root, &metadata).ok_or(PurgeFileFault::Unsupported)?;
    let root_path = std::fs::canonicalize(root).map_err(|_| PurgeFileFault::Io)?;
    let read_root = ReadRoot {
        location: root_path,
        identity: Some(root_identity),
    };
    if super::normal_components(relative).is_none() {
        return Err(PurgeFileFault::Changed);
    }
    let path = joined_relative(&read_root.location, relative)?;
    let before = std::fs::symlink_metadata(&path).map_err(|_| PurgeFileFault::Changed)?;
    if !before.is_file() {
        return Err(PurgeFileFault::Changed);
    }
    let identity = identity_at(&path, &before).ok_or(PurgeFileFault::Unsupported)?;
    let mut opened = NativeFilesystem::new()
        .open(&ReadRequest {
            root: &read_root,
            relative,
            max_bytes: before.len(),
            allow_hard_links: false,
        })
        .map_err(read_fault)?;
    if opened.identity != Some(identity) {
        return Err(PurgeFileFault::Changed);
    }
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut bytes = [0u8; 64 * 1024];
    loop {
        if stopped() {
            return Err(PurgeFileFault::Cancelled);
        }
        let n = opened
            .reader
            .read(&mut bytes)
            .map_err(|_| PurgeFileFault::Io)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).ok_or(PurgeFileFault::Changed)?;
        if count > before.len() {
            return Err(PurgeFileFault::Changed);
        }
        hasher.update(&bytes[..n]);
    }
    let after = std::fs::symlink_metadata(&path).map_err(|_| PurgeFileFault::Changed)?;
    if count != before.len()
        || after.len() != before.len()
        || identity_at(&path, &after) != Some(identity)
        || before.modified().ok() != after.modified().ok()
    {
        return Err(PurgeFileFault::Changed);
    }
    Ok(PurgeFileWitness {
        project_id: binding.project_id.clone(),
        store_id: binding.store_id.clone(),
        restore_epoch: binding.restore_epoch,
        owner,
        root: root_identity,
        relative_entry: relative.into(),
        identity,
        byte_count: count,
        content_digest: hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        directory: false,
        link: false,
    })
}

use devworks_core::ports::project_purge::{
    ProjectPurgeFiles, PurgeFileSelection, PurgeFileSnapshot,
};
use devworks_core::ports::storage::{ApplicationStore, ReadOptions, read_snapshot};
use std::path::PathBuf;
use std::sync::Arc;

/// Exact managed roots of the native application; no caller filesystem path is accepted.
pub struct NativeProjectPurgeFiles {
    data_root: PathBuf,
    cache_root: PathBuf,
    paste_root: Option<PathBuf>,
    store: Arc<dyn ApplicationStore>,
}
impl NativeProjectPurgeFiles {
    pub fn new(data_root: PathBuf, cache_root: PathBuf, store: Arc<dyn ApplicationStore>) -> Self {
        Self {
            data_root,
            cache_root,
            paste_root: None,
            store,
        }
    }
    pub fn with_paste_root(mut self, root: PathBuf) -> Self {
        self.paste_root = Some(root);
        self
    }
    fn binding(&self, project_id: devworks_core::protocol::ProjectId) -> PurgeFileSelection {
        let info = self.store.info();
        PurgeFileSelection {
            project_id,
            store_id: info.store_id,
            restore_epoch: info.restore_epoch,
            owners: Vec::new(),
        }
    }
    fn registered(
        &self,
        owner: &PurgeFileOwner,
        binding: &PurgeFileSelection,
    ) -> Result<Option<PurgeFileWitness>, PurgeFileFault> {
        let file_id = match owner {
            PurgeFileOwner::DiagnosticExport { file_id }
            | PurgeFileOwner::ClientPaste { file_id }
            | PurgeFileOwner::ClientStaging { file_id } => file_id,
            _ => return Ok(None),
        };
        let file = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |r| {
            r.projects()
                .project_managed_file(&binding.project_id, file_id)
        })
        .map_err(|_| PurgeFileFault::Io)?
        .ok_or(PurgeFileFault::Unattributed)?;
        if file.witness.owner != *owner
            || file.witness.project_id != binding.project_id
            || file.witness.store_id != binding.store_id
            || file.witness.restore_epoch != binding.restore_epoch
        {
            return Err(PurgeFileFault::Unattributed);
        }
        Ok(Some(file.witness))
    }
    fn root(&self, owner: &PurgeFileOwner) -> Result<Option<PathBuf>, PurgeFileFault> {
        let (base, parts): (PathBuf, Vec<String>) = match owner {
            PurgeFileOwner::Blob { digest, generation } => {
                valid_blob(digest, generation)?;
                (
                    self.data_root.clone(),
                    vec!["artifacts".into(), "sha256".into(), digest[..2].into()],
                )
            }
            PurgeFileOwner::ArtifactStaging { operation_id } => {
                valid_id(operation_id.as_str())?;
                (self.data_root.clone(), vec!["staging".into()])
            }
            PurgeFileOwner::Quarantine { digest, generation } => {
                valid_blob(digest, generation)?;
                (self.data_root.clone(), vec!["quarantine".into()])
            }
            PurgeFileOwner::Preview { version_id } => {
                valid_id(version_id.as_str())?;
                (self.cache_root.clone(), vec!["artifact-previews".into()])
            }
            PurgeFileOwner::BrowserProfile { session_id } => {
                valid_id(session_id.as_str())?;
                (
                    self.data_root.clone(),
                    vec!["browser".into(), "sessions".into()],
                )
            }
            PurgeFileOwner::DiagnosticExport { .. } => (
                self.data_root.clone(),
                vec!["diagnostics".into(), "exports".into()],
            ),
            PurgeFileOwner::ClientPaste { .. } | PurgeFileOwner::ClientStaging { .. } => (
                self.paste_root
                    .clone()
                    .ok_or(PurgeFileFault::Unattributed)?,
                Vec::new(),
            ),
        };
        checked_root(&base, &parts)
    }
    fn candidates(
        &self,
        owner: &PurgeFileOwner,
        binding: &PurgeFileSelection,
        root: &Path,
    ) -> Result<Vec<String>, PurgeFileFault> {
        let exact = match owner {
            PurgeFileOwner::Blob { digest, generation } => Some(format!("{digest}.{generation}")),
            PurgeFileOwner::ArtifactStaging { operation_id } => {
                Some(format!("{}.part", operation_id.as_str()))
            }
            PurgeFileOwner::BrowserProfile { session_id } => Some(session_id.as_str().to_owned()),
            PurgeFileOwner::DiagnosticExport { file_id }
            | PurgeFileOwner::ClientPaste { file_id }
            | PurgeFileOwner::ClientStaging { file_id } => {
                let _ = file_id;
                Some(
                    self.registered(owner, binding)?
                        .ok_or(PurgeFileFault::Unattributed)?
                        .relative_entry,
                )
            }
            _ => None,
        };
        if let Some(name) = exact {
            return Ok(vec![name]);
        }
        let (key, prefix, temporary_only) = match owner {
            PurgeFileOwner::Quarantine { digest, generation } => {
                (String::new(), format!("{digest}.{generation}."), false)
            }
            PurgeFileOwner::Preview { version_id } => {
                let key =
                    devworks_core::services::artifacts::preview::preview_id(version_id.as_str());
                (key.clone(), format!("{key}."), true)
            }
            _ => return Err(PurgeFileFault::Unsupported),
        };
        let mut names = Vec::new();
        for entry in std::fs::read_dir(root).map_err(|_| PurgeFileFault::Io)? {
            let entry = entry.map_err(|_| PurgeFileFault::Io)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| PurgeFileFault::Unattributed)?;
            if name == key || name.starts_with(&prefix) {
                if name != key {
                    let suffix = name
                        .strip_prefix(&prefix)
                        .ok_or(PurgeFileFault::Unattributed)?;
                    let suffix = if temporary_only {
                        suffix
                            .strip_suffix(".tmp")
                            .ok_or(PurgeFileFault::Unattributed)?
                    } else {
                        suffix
                    };
                    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                    {
                        return Err(PurgeFileFault::Unattributed);
                    }
                }
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
    fn capture_entry(
        &self,
        root: &Path,
        binding: &PurgeFileSelection,
        relative: &str,
        owner: &PurgeFileOwner,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Option<PurgeFileWitness>, PurgeFileFault> {
        use devworks_core::authority::destructive::{EntryKind, FilesystemResolver};
        if stopped() {
            return Err(PurgeFileFault::Cancelled);
        }
        if super::normal_components(relative).is_none() {
            return Err(PurgeFileFault::Changed);
        }
        let path = joined_relative(root, relative)?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(PurgeFileFault::Io),
        };
        if metadata.is_file() && !super::reparse(&metadata) {
            let witness = capture_managed_file(root, binding, relative, owner.clone(), stopped)?;
            if self
                .registered(owner, binding)?
                .is_some_and(|published| published != witness)
            {
                return Err(PurgeFileFault::Changed);
            }
            return Ok(Some(witness));
        }
        let resolved = NativeFilesystem::new()
            .resolve(path.to_str().ok_or(PurgeFileFault::Unsupported)?, None)
            .map_err(|_| PurgeFileFault::Changed)?;
        if !resolved.linked_ancestors.is_empty() {
            return Err(PurgeFileFault::Changed);
        }
        let directory = resolved.entry_kind == EntryKind::Directory;
        let link = matches!(
            resolved.entry_kind,
            EntryKind::Symlink | EntryKind::ReparsePoint
        );
        if !directory && !link {
            return Err(PurgeFileFault::Unsupported);
        }
        let root_metadata = std::fs::symlink_metadata(root).map_err(|_| PurgeFileFault::Changed)?;
        let root_id = identity_at(root, &root_metadata).ok_or(PurgeFileFault::Unsupported)?;
        let identity = resolved.entry.identity.ok_or(PurgeFileFault::Unsupported)?;
        if identity.volume != root_id.volume {
            return Err(PurgeFileFault::Unattributed);
        }
        let digest = if link {
            let target = std::fs::read_link(&path).map_err(|_| PurgeFileFault::Changed)?;
            link_digest(&target)
        } else {
            directory_digest(&path)?
        };
        Ok(Some(PurgeFileWitness {
            project_id: binding.project_id.clone(),
            store_id: binding.store_id.clone(),
            restore_epoch: binding.restore_epoch,
            owner: owner.clone(),
            root: root_id,
            relative_entry: relative.into(),
            identity,
            byte_count: 0,
            content_digest: digest,
            directory,
            link,
        }))
    }
}

fn directory_digest(path: &Path) -> Result<String, PurgeFileFault> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(path).map_err(|_| PurgeFileFault::Io)? {
        let item = item.map_err(|_| PurgeFileFault::Io)?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| PurgeFileFault::Unattributed)?;
        let metadata =
            std::fs::symlink_metadata(item.path()).map_err(|_| PurgeFileFault::Changed)?;
        let identity = identity_at(&item.path(), &metadata).ok_or(PurgeFileFault::Unsupported)?;
        entries.push((name, identity.volume, identity.file));
    }
    entries.sort();
    Ok(devworks_core::fingerprint::sha256_hex(
        serde_json::to_string(&entries)
            .map_err(|_| PurgeFileFault::Io)?
            .as_bytes(),
    ))
}
fn owner_relative(owner: &PurgeFileOwner, relative: &str) -> bool {
    if super::normal_components(relative).is_none() {
        return false;
    }
    match owner {
        PurgeFileOwner::Blob { digest, generation } => relative == format!("{digest}.{generation}"),
        PurgeFileOwner::ArtifactStaging { operation_id } => {
            relative == format!("{}.part", operation_id.as_str())
        }
        PurgeFileOwner::BrowserProfile { session_id } => {
            relative == session_id.as_str()
                || relative.starts_with(&format!("{}/", session_id.as_str()))
        }
        PurgeFileOwner::Quarantine { digest, generation } => relative
            .strip_prefix(&format!("{digest}.{generation}."))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-')),
        PurgeFileOwner::Preview { version_id } => {
            let key = devworks_core::services::artifacts::preview::preview_id(version_id.as_str());
            relative == key
                || relative
                    .strip_prefix(&format!("{key}."))
                    .and_then(|s| s.strip_suffix(".tmp"))
                    .is_some_and(|s| {
                        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'-')
                    })
        }
        PurgeFileOwner::DiagnosticExport { .. }
        | PurgeFileOwner::ClientPaste { .. }
        | PurgeFileOwner::ClientStaging { .. } => {
            std::path::Path::new(relative).components().count() == 1
        }
    }
}

fn valid_id(id: &str) -> Result<(), PurgeFileFault> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(())
    } else {
        Err(PurgeFileFault::Unattributed)
    }
}
fn valid_blob(digest: &str, generation: &str) -> Result<(), PurgeFileFault> {
    valid_id(generation)?;
    if digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(PurgeFileFault::Unattributed)
    }
}
fn checked_root(base: &Path, parts: &[String]) -> Result<Option<PathBuf>, PurgeFileFault> {
    let meta = match std::fs::symlink_metadata(base) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(PurgeFileFault::Io),
    };
    if !meta.is_dir() || super::reparse(&meta) {
        return Err(PurgeFileFault::Changed);
    }
    let mut path = std::fs::canonicalize(base).map_err(|_| PurgeFileFault::Io)?;
    for part in parts {
        path.push(part);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(PurgeFileFault::Io),
        };
        if !meta.is_dir() || super::reparse(&meta) {
            return Err(PurgeFileFault::Changed);
        }
    }
    Ok(Some(path))
}
fn link_digest(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        devworks_core::fingerprint::sha256_hex(path.as_os_str().as_bytes())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        devworks_core::fingerprint::sha256_hex(&bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        devworks_core::fingerprint::sha256_hex(path.to_string_lossy().as_bytes())
    }
}

impl ProjectPurgeFiles for NativeProjectPurgeFiles {
    fn inventory(
        &self,
        selection: &PurgeFileSelection,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<PurgeFileSnapshot, PurgeFileFault> {
        let expected = self.binding(selection.project_id.clone());
        if expected.store_id != selection.store_id
            || expected.restore_epoch != selection.restore_epoch
        {
            return Err(PurgeFileFault::Changed);
        }
        let mut files = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for owner in &selection.owners {
            let Some(root) = self.root(owner)? else {
                continue;
            };
            let mut pending = self.candidates(owner, selection, &root)?;
            while let Some(relative) = pending.pop() {
                let Some(witness) =
                    self.capture_entry(&root, selection, &relative, owner, stopped)?
                else {
                    continue;
                };
                if !seen.insert((witness.root.volume, witness.root.file, relative.clone())) {
                    continue;
                }
                if witness.directory {
                    if !matches!(owner, PurgeFileOwner::BrowserProfile { .. }) {
                        return Err(PurgeFileFault::Unattributed);
                    }
                    for child in std::fs::read_dir(joined_relative(&root, &relative)?)
                        .map_err(|_| PurgeFileFault::Io)?
                    {
                        let child = child.map_err(|_| PurgeFileFault::Io)?;
                        let name = child
                            .file_name()
                            .into_string()
                            .map_err(|_| PurgeFileFault::Unattributed)?;
                        pending.push(format!("{relative}/{name}"));
                    }
                }
                files.push(witness);
            }
        }
        files.sort_by_key(|w| {
            (
                serde_json::to_string(&w.owner).expect("file owner JSON"),
                w.relative_entry.clone(),
            )
        });
        // Recheck the completed enumeration, including every directory identity.
        for witness in &files {
            let root = self.root(&witness.owner)?.ok_or(PurgeFileFault::Changed)?;
            if self
                .capture_entry(
                    &root,
                    selection,
                    &witness.relative_entry,
                    &witness.owner,
                    stopped,
                )?
                .as_ref()
                != Some(witness)
            {
                return Err(PurgeFileFault::Changed);
            }
        }
        let digest=devworks_core::fingerprint::sha256_hex(serde_json::to_string(&serde_json::json!({"projectId":selection.project_id,"storeId":selection.store_id,"restoreEpoch":selection.restore_epoch,"owners":selection.owners,"files":files})).map_err(|_|PurgeFileFault::Io)?.as_bytes());
        Ok(PurgeFileSnapshot {
            selection: selection.clone(),
            files,
            digest,
        })
    }
    fn remove_reviewed(
        &self,
        witness: &PurgeFileWitness,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<(), PurgeFileFault> {
        if !owner_relative(&witness.owner, &witness.relative_entry) {
            return Err(PurgeFileFault::Unattributed);
        }
        let binding = self.binding(witness.project_id.clone());
        if binding.store_id != witness.store_id || binding.restore_epoch != witness.restore_epoch {
            return Err(PurgeFileFault::Changed);
        }
        let Some(root) = self.root(&witness.owner)? else {
            return Ok(());
        };
        if let Some(mut current) = self.capture_entry(
            &root,
            &binding,
            &witness.relative_entry,
            &witness.owner,
            stopped,
        )? {
            // The reviewed children are intentionally removed first. Directory identity
            // remains bound, and the native empty-directory operation refuses new entries.
            if witness.directory {
                current.content_digest = witness.content_digest.clone();
            }
            if current != *witness {
                return Err(PurgeFileFault::Changed);
            }
        } else {
            return Ok(());
        }
        remove_native(&root, witness, stopped)
    }
}

fn io_fault(error: std::io::Error) -> PurgeFileFault {
    match error.kind() {
        std::io::ErrorKind::NotFound => PurgeFileFault::Changed,
        std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::DirectoryNotEmpty => PurgeFileFault::InUse,
        _ => PurgeFileFault::Io,
    }
}
fn verify_open_content(
    file: &mut std::fs::File,
    w: &PurgeFileWitness,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<(), PurgeFileFault> {
    let mut hasher = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        if stopped() {
            return Err(PurgeFileFault::Cancelled);
        }
        let n = file.read(&mut bytes).map_err(io_fault)?;
        if n == 0 {
            break;
        }
        total = total.checked_add(n as u64).ok_or(PurgeFileFault::Changed)?;
        if total > w.byte_count {
            return Err(PurgeFileFault::Changed);
        }
        hasher.update(&bytes[..n]);
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if total == w.byte_count && digest == w.content_digest {
        Ok(())
    } else {
        Err(PurgeFileFault::Changed)
    }
}

#[cfg(unix)]
pub(super) fn remove_native(
    root: &Path,
    w: &PurgeFileWitness,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<(), PurgeFileFault> {
    use super::unix as native;
    use std::os::fd::AsRawFd;
    let parts = super::normal_components(&w.relative_entry).ok_or(PurgeFileFault::Changed)?;
    let mut dir = native::open_dir_nofollow(root).map_err(io_fault)?;
    if native::ident(&native::fstat(dir.as_raw_fd()).map_err(io_fault)?) != w.root {
        return Err(PurgeFileFault::Changed);
    }
    for part in &parts[..parts.len() - 1] {
        dir = native::open_at(
            dir.as_raw_fd(),
            &native::cname(part).map_err(io_fault)?,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
        .map_err(io_fault)?;
        if native::ident(&native::fstat(dir.as_raw_fd()).map_err(io_fault)?).volume != w.root.volume
        {
            return Err(PurgeFileFault::Changed);
        }
    }
    let name = native::cname(parts.last().ok_or(PurgeFileFault::Changed)?).map_err(io_fault)?;
    let before = match native::stat_at(dir.as_raw_fd(), &name) {
        Ok(st) => st,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_fault(e)),
    };
    if native::ident(&before) != w.identity
        || native::is_link(&before) != w.link
        || native::is_dir(&before) != w.directory
    {
        return Err(PurgeFileFault::Changed);
    }
    let opened = if !w.directory && !w.link {
        let descriptor = native::open_at(
            dir.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
        .map_err(io_fault)?;
        if native::ident(&native::fstat(descriptor.as_raw_fd()).map_err(io_fault)?) != w.identity {
            return Err(PurgeFileFault::Changed);
        }
        let mut file = std::fs::File::from(descriptor);
        verify_open_content(&mut file, w, stopped)?;
        Some(file)
    } else {
        None
    };
    if stopped() {
        return Err(PurgeFileFault::Cancelled);
    }
    let current = native::stat_at(dir.as_raw_fd(), &name).map_err(io_fault)?;
    if native::ident(&current) != w.identity {
        return Err(PurgeFileFault::Changed);
    }
    native::unlink_at(dir.as_raw_fd(), &name, w.directory).map_err(io_fault)?;
    drop(opened);
    let parent = std::fs::File::from(dir);
    parent.sync_all().map_err(io_fault)
}

#[cfg(windows)]
pub(super) fn remove_native(
    root: &Path,
    w: &PurgeFileWitness,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<(), PurgeFileFault> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    let parts = super::normal_components(&w.relative_entry).ok_or(PurgeFileFault::Changed)?;
    if parts
        .iter()
        .any(|p| super::windows_unsafe_component(&p.to_string_lossy()))
    {
        return Err(PurgeFileFault::Changed);
    }
    let mut held = Vec::new();
    let initial = super::windows::open_held(root).map_err(io_fault)?;
    if super::windows::file_identity(&initial) != Some(w.root) {
        return Err(PurgeFileFault::Changed);
    }
    if initial.metadata().map_err(io_fault)?.file_attributes() & 0x400 != 0 {
        return Err(PurgeFileFault::Changed);
    }
    held.push(initial);
    let mut path = root.to_owned();
    for part in &parts[..parts.len() - 1] {
        path.push(part);
        let dir = super::windows::open_held(&path).map_err(io_fault)?;
        let meta = dir.metadata().map_err(io_fault)?;
        if !meta.is_dir()
            || meta.file_attributes() & 0x400 != 0
            || super::windows::file_identity(&dir).is_none_or(|i| i.volume != w.root.volume)
        {
            return Err(PurgeFileFault::Changed);
        }
        held.push(dir);
    }
    path.push(parts.last().ok_or(PurgeFileFault::Changed)?);
    let mut options = std::fs::OpenOptions::new();
    // DELETE | GENERIC_READ; ancestors and target are opened without delete sharing.
    options
        .access_mode(0x0001_0000 | 0x8000_0000)
        .share_mode(0x1 | 0x2)
        .custom_flags(0x0200_0000 | 0x0020_0000);
    let mut file = match options.open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io_fault(e)),
    };
    let meta = file.metadata().map_err(io_fault)?;
    let link = meta.file_attributes() & 0x400 != 0;
    if super::windows::file_identity(&file) != Some(w.identity)
        || link != w.link
        || (!link && meta.is_dir()) != w.directory
    {
        return Err(PurgeFileFault::Changed);
    }
    if !w.directory && !w.link {
        verify_open_content(&mut file, w, stopped)?
    }
    if stopped() {
        return Err(PurgeFileFault::Cancelled);
    }
    #[repr(C)]
    struct Disposition {
        delete_file: u8,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            handle: *mut core::ffi::c_void,
            class: i32,
            information: *const core::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let disposition = Disposition { delete_file: 1 };
    // Microsoft FILE_DISPOSITION_INFO / FileDispositionInfo=4. The same verified
    // DELETE-capable handle is marked; no pathname is reopened for the deletion.
    // https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle
    // DeleteFile is a one-byte BOOLEAN, not a four-byte BOOL:
    // https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_disposition_info
    // SAFETY: live owned handle, correct C structure and initialized buffer size.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast(),
            4,
            (&disposition as *const Disposition).cast(),
            std::mem::size_of::<Disposition>() as u32,
        )
    } == 0
    {
        return Err(io_fault(std::io::Error::last_os_error()));
    }
    drop(file);
    drop(held);
    Ok(())
}
#[cfg(not(any(unix, windows)))]
pub(super) fn remove_native(
    _root: &Path,
    _w: &PurgeFileWitness,
    _stopped: &mut dyn FnMut() -> bool,
) -> Result<(), PurgeFileFault> {
    Err(PurgeFileFault::Unsupported)
}
