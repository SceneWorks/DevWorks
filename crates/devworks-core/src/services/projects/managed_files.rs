//! Custody of host-created project files: reservation, the owner scope check, publication and
//! recovery of interrupted writes. Native adapters perform only the filesystem I/O behind
//! `ports::managed_files` and `ports::files`; every store transaction is here.
use crate::authority::destructive::FileIdentity;
use crate::ports::IdKind;
use crate::ports::files::{ArtifactFiles, FileFault};
use crate::ports::managed_files::{ProjectFileFence, PurgeFileRecords, StagingDirectory};
use crate::ports::project_purge::{
    PurgeFileFault, PurgeFileOwner, PurgeFileSelection, PurgeFileWitness,
};
use crate::ports::storage::{
    ApplicationStore, ProjectManagedFileRecovery, ProjectManagedFileReservation, ReadOptions,
    StorageError, StoredProjectManagedFile, WriteOptions, read_snapshot, run_write,
};
use crate::protocol::{Id, ProjectId};
use std::path::PathBuf;
use std::sync::Arc;

/// Who a client staging file belongs to. A session owner's project comes from the stored
/// session, never from the caller.
#[derive(Clone, Debug)]
pub enum StagingOwner {
    Session(Option<Id>),
    Project(Option<ProjectId>),
}

/// The two kinds of client staging file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingKind {
    /// A pasted image.
    ClientPaste,
    /// Text staged from a native editor.
    ClientStaging,
}
impl StagingKind {
    fn name(self) -> &'static str {
        match self {
            Self::ClientPaste => "ClientPaste",
            Self::ClientStaging => "ClientStaging",
        }
    }
    fn owner(self, id: &Id) -> PurgeFileOwner {
        match self {
            Self::ClientPaste => PurgeFileOwner::ClientPaste {
                file_id: id.clone(),
            },
            Self::ClientStaging => PurgeFileOwner::ClientStaging {
                file_id: id.clone(),
            },
        }
    }
}

const PREVIEW_KIND: &str = "Preview";

pub struct ManagedFileCustody {
    store: Arc<dyn ApplicationStore>,
}

impl ManagedFileCustody {
    pub fn new(store: Arc<dyn ApplicationStore>) -> Self {
        Self { store }
    }

    fn selection(&self, project_id: ProjectId) -> PurgeFileSelection {
        let info = self.store.info();
        PurgeFileSelection {
            project_id,
            store_id: info.store_id,
            restore_epoch: info.restore_epoch,
            owners: Vec::new(),
        }
    }

    /// Visit every recovery row of `kind` an earlier boot left writing.
    fn abandoned<E>(
        &self,
        kind: &'static str,
        unavailable: impl Fn() -> E,
        mut settle: impl FnMut(ProjectManagedFileRecovery) -> Result<(), E>,
    ) -> Result<(), E> {
        let boot = self.store.info().boot_id;
        let mut after = None;
        loop {
            let page = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |r| {
                r.projects()
                    .managed_file_recovery(kind, after.as_ref(), 500)
            })
            .map_err(|_| unavailable())?;
            if page.is_empty() {
                return Ok(());
            }
            after = page.last().map(|r| r.id.clone());
            for row in page {
                if !row.writing || row.boot_id == boot {
                    continue;
                }
                settle(row)?;
            }
        }
    }

    fn finish(&self, project: ProjectId, id: Id) -> Result<(), StorageError> {
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            tx.projects().finish_project_managed_file(&project, &id)
        })
    }

    /// Stage a pasted PNG for `session`'s stored project.
    pub fn stage_paste(
        &self,
        directory: &dyn StagingDirectory,
        session: Option<&Id>,
        id: &Id,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        self.stage_client_file(
            directory,
            StagingOwner::Session(session.cloned()),
            id,
            StagingKind::ClientPaste,
            "png",
            bytes,
        )
    }

    /// Stage native editor text (`md`, `txt` or `json`) for `project`.
    pub fn stage_project_text(
        &self,
        directory: &dyn StagingDirectory,
        project: Option<&ProjectId>,
        id: &Id,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        self.stage_client_file(
            directory,
            StagingOwner::Project(project.cloned()),
            id,
            StagingKind::ClientStaging,
            extension,
            bytes,
        )
    }

    /// Reserve before creation, bind the create-new identity before bytes, then publish.
    /// A failed project write stays durably Writing for the staging owner's recovery. A file
    /// with no project owner is written without custody.
    pub fn stage_client_file(
        &self,
        directory: &dyn StagingDirectory,
        owner: StagingOwner,
        id: &Id,
        kind: StagingKind,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        if kind == StagingKind::ClientStaging && !matches!(extension, "md" | "txt" | "json") {
            return Err("unsupported native staging format".into());
        }
        let relative = format!("{}.{extension}", id.as_str());
        let mut write = directory.open(&relative)?;
        let root = write.root();
        let reserved_id = id.clone();
        let reserved_path = relative.clone();
        let reservation = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let project = match owner {
                StagingOwner::Project(project) => project,
                StagingOwner::Session(None) => None,
                StagingOwner::Session(Some(id)) => {
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
                kind: kind.name().into(),
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

        if let Err(error) = write.create() {
            if let Some(reserved) = reservation {
                // This call never obtained a file: a colliding entry is independent.
                self.finish(reserved.project_id, reserved.id)
                    .map_err(|_| "staging reservation settlement is unavailable".to_owned())?;
            }
            return Err(error);
        }
        let identity = write.created()?;
        if let Some(reserved) = &reservation {
            let project = reserved.project_id.clone();
            let id = id.clone();
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects()
                    .begin_project_managed_file_write(&project, &id, identity)
            })
            .map_err(|_| "staged-file creation could not be recorded".to_owned())?;
        }
        write.write(bytes)?;

        if let Some(reserved) = reservation {
            let binding = PurgeFileSelection {
                project_id: reserved.project_id.clone(),
                store_id: reserved.store_id.clone(),
                restore_epoch: reserved.restore_epoch,
                owners: Vec::new(),
            };
            let witness = write
                .witness(&binding, kind.owner(id))
                .map_err(|_| "staged-file publication witness changed".to_owned())?;
            if witness.root != root || witness.identity != identity {
                return Err("staged-file publication identity changed".into());
            }
            let published = StoredProjectManagedFile {
                id: id.clone(),
                project_id: reserved.project_id,
                kind: reserved.kind,
                relative_path: relative,
                witness,
                created_at_ms: reserved.created_at_ms,
            };
            run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().register_project_managed_file(&published)
            })
            .map_err(|_| "staged-file publication could not be recorded".to_owned())?;
        }
        Ok(write.path())
    }

    /// Settle only incomplete writes of a previous boot: no path was returned to a draft.
    /// Live paste files require the client keeper snapshot and are never swept here.
    pub fn recover_client_staging(&self, directory: &dyn StagingDirectory) -> Result<(), String> {
        for kind in [StagingKind::ClientPaste, StagingKind::ClientStaging] {
            self.abandoned(
                kind.name(),
                || "staging recovery inventory is unavailable".to_owned(),
                |row| {
                    let binding = self.selection(row.project_id.clone());
                    if row.store_id != binding.store_id
                        || row.restore_epoch != binding.restore_epoch
                    {
                        return Err("staging recovery store or restore epoch changed".into());
                    }
                    // No bound identity means the producer could not yet write private bytes.
                    // An unbound existing entry is independent and is never removed.
                    if let Some(identity) = row.file_identity {
                        directory.remove_abandoned(
                            &row,
                            identity,
                            &binding,
                            kind.owner(&row.id),
                        )?;
                    }
                    self.finish(row.project_id, row.id)
                        .map_err(|_| "staging recovery settlement is unavailable".to_owned())
                },
            )?;
        }
        Ok(())
    }

    /// Project previews cannot create private cache bytes after a reviewed purge: the
    /// reservation is refused once the project is fenced, and the preview stays uncached.
    pub fn write_project_preview(
        &self,
        files: &dyn ArtifactFiles,
        project: &ProjectId,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), FileFault> {
        if !key.starts_with("pv-")
            || key.len() != 43
            || !key[3..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(FileFault::Unsupported(
                "invalid project preview identity".into(),
            ));
        }
        let root: FileIdentity = files.derived_root()?;
        let project = project.clone();
        let relative_path = key.to_owned();
        let reserved = run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let info = tx.store().clone();
            let reserved = ProjectManagedFileReservation {
                id: tx.new_id(IdKind::Artifact),
                project_id: project,
                kind: PREVIEW_KIND.into(),
                relative_path,
                root,
                created_identity: None,
                store_id: info.store_id,
                restore_epoch: info.restore_epoch,
                boot_id: info.boot_id,
                created_at_ms: tx.now_ms(),
            };
            tx.projects().reserve_project_managed_file(&reserved)?;
            Ok(reserved)
        })
        .map_err(|_| FileFault::Unsupported("project cache write closed".into()))?;
        let result = files.write_derived(key, bytes);
        if self.finish(reserved.project_id, reserved.id).is_err() {
            return Err(FileFault::Io("project cache settlement unavailable".into()));
        }
        result
    }

    /// Normal cache-owner recovery, never a purge-triggered global cache sweep.
    pub fn recover_project_previews(&self, files: &dyn ArtifactFiles) -> Result<(), FileFault> {
        self.abandoned(
            PREVIEW_KIND,
            || FileFault::Io("cache recovery unavailable".into()),
            |row| {
                files.remove_derived_remnants(&row.relative_path, row.root)?;
                self.finish(row.project_id, row.id)
                    .map_err(|_| FileFault::Io("cache recovery settlement unavailable".into()))
            },
        )
    }
}

impl PurgeFileRecords for ManagedFileCustody {
    fn binding(&self, project_id: ProjectId) -> PurgeFileSelection {
        self.selection(project_id)
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

    fn effect_reviewed(&self, witness: &PurgeFileWitness) -> Result<bool, PurgeFileFault> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            read.projects().purge_file_effect_reviewed(witness)
        })
        .map_err(|_| PurgeFileFault::Io)
    }
}

impl ProjectFileFence for ManagedFileCustody {
    fn open(&self, project: &ProjectId) -> bool {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            read.projects().purge_fenced(project)
        })
        .is_ok_and(|fenced| !fenced)
    }
}
