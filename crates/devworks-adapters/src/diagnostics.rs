//! The production [`ExportStaging`] for diagnostic exports: the rendered, redacted export is
//! written to a host-owned directory (under the data root's `diagnostics/exports`) and minted
//! as an upload ticket of the exporting person ([`HostArtifactSources::mint_upload`]), which the
//! export's `artifacts.save` then resolves like any file the user picked. The ticket is bound to
//! that person, the invocation's project and work root, and expires; an agent, package, MCP
//! client or schedule cannot mint one. A staged file is removed as soon as its save is decided
//! ([`ExportStaging::release`]); one whose outcome stayed unknown is removed by the sweep once
//! older than the ticket lifetime ([`ExportStaging::sweep`], run by the runtime's diagnostics
//! monitor). The artifact keeps its own copy.
use crate::fs::sources::{HostArtifactSources, IssueRefusal};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::diagnostics::{ExportRefused, ExportStaging};
use devworks_core::ports::project_purge::PurgeFileOwner;
use devworks_core::ports::storage::{
    ApplicationStore, ProjectManagedFileReservation, ReadOptions, StoredProjectManagedFile,
    WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::{Id, ProjectId};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Lifetime of an export's upload ticket.
pub const EXPORT_TICKET_MS: i64 = 10 * 60 * 1000;

/// Removes a released export. Windows briefly refuses to delete a file another process (a
/// scanner or indexer) still has open, so a refusal is retried for a moment; a file that is
/// still there is left to the sweep.
fn remove_staged(path: &Path) {
    for attempt in 1..=10 {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound && attempt < 10 => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => return,
        }
    }
}

/// Stages diagnostic exports as upload tickets (see the module documentation).
pub struct DiagnosticExports {
    dir: PathBuf,
    sources: Arc<HostArtifactSources>,
    next: AtomicU64,
    /// Staged files by upload ticket, until released.
    staged: Mutex<HashMap<String, PathBuf>>,
    store: Option<Arc<dyn ApplicationStore>>,
    managed: Mutex<HashMap<String, (ProjectId, Id)>>,
}

/// Failure cleanup remains bound to the actual newly opened file. A replaced root
/// or entry is left held; it never inherits this attempt's deletion authority.
struct StagingAttempt<'a> {
    owner: &'a DiagnosticExports,
    reservation: Option<ProjectManagedFileReservation>,
    path: PathBuf,
    identity: Option<devworks_core::authority::destructive::FileIdentity>,
    committed: bool,
}
impl Drop for StagingAttempt<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let missing = matches!(std::fs::symlink_metadata(&self.path),Err(e) if e.kind()==std::io::ErrorKind::NotFound);
        let same_root = self.reservation.as_ref().is_none_or(|r| {
            std::fs::symlink_metadata(&self.owner.dir)
                .ok()
                .and_then(|m| crate::fs::identity_at(&self.owner.dir, &m))
                == Some(r.root)
        });
        if !missing
            && same_root
            && self.identity.is_some()
            && std::fs::symlink_metadata(&self.path)
                .ok()
                .and_then(|m| crate::fs::identity_at(&self.path, &m))
                == self.identity
        {
            remove_staged(&self.path)
        }
        if (missing
            || matches!(std::fs::symlink_metadata(&self.path),Err(e) if e.kind()==std::io::ErrorKind::NotFound))
            && let (Some(store), Some(r)) = (&self.owner.store, &self.reservation)
        {
            let project = r.project_id.clone();
            let id = r.id.clone();
            let _ = run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().finish_project_managed_file(&project, &id)
            });
        }
    }
}

impl DiagnosticExports {
    /// `dir`: a host-owned directory (normally `<data root>/diagnostics/exports`).
    pub fn new(dir: impl Into<PathBuf>, sources: Arc<HostArtifactSources>) -> Self {
        Self {
            dir: dir.into(),
            sources,
            next: AtomicU64::new(0),
            staged: Mutex::new(HashMap::new()),
            store: None,
            managed: Mutex::new(HashMap::new()),
        }
    }

    /// Production attribution is reserved before external creation and survives restart.
    pub fn with_store(mut self, store: Arc<dyn ApplicationStore>) -> Self {
        self.store = Some(store);
        self
    }

    fn settle_managed(&self, ticket: &str) {
        if let Some((project, id)) = self
            .managed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(ticket)
            && let Some(store) = &self.store
        {
            let _ = run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().finish_project_managed_file(&project, &id)
            });
        }
    }
    fn reconcile_missing(&self) {
        let Some(store) = &self.store else { return };
        let Some(root) = std::fs::symlink_metadata(&self.dir)
            .ok()
            .and_then(|m| crate::fs::identity_at(&self.dir, &m))
        else {
            return;
        };
        let mut after = None;
        loop {
            let Ok(rows) = read_snapshot(store.as_ref(), &ReadOptions::default(), |r| {
                r.projects()
                    .managed_file_recovery("DiagnosticExport", after.as_ref(), 500)
            }) else {
                return;
            };
            if rows.is_empty() {
                return;
            }
            after = rows.last().map(|r| r.id.clone());
            for row in rows {
                if row.root == root
                    && matches!(std::fs::symlink_metadata(self.dir.join(&row.relative_path)),Err(e) if e.kind()==std::io::ErrorKind::NotFound)
                {
                    let _ = run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                        tx.projects()
                            .finish_project_managed_file(&row.project_id, &row.id)
                    });
                }
            }
        }
    }
    fn staged(&self) -> std::sync::MutexGuard<'_, HashMap<String, PathBuf>> {
        self.staged.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ExportStaging for DiagnosticExports {
    fn release(&self, source: &ArtifactSource) {
        let ArtifactSource::Upload { ticket_id } = source else {
            return;
        };
        if let Some(path) = self.staged().remove(ticket_id.as_str()) {
            remove_staged(&path);
            if !path.exists() {
                self.settle_managed(ticket_id.as_str());
            }
        }
    }

    fn sweep(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let cutoff = Duration::from_millis(EXPORT_TICKET_MS as u64);
        let mut removed = 0;
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age > cutoff);
            if old
                && e.file_type().is_ok_and(|t| t.is_file())
                && std::fs::remove_file(e.path()).is_ok()
            {
                removed += 1;
            }
        }
        let missing = self
            .staged()
            .iter()
            .filter(|(_, path)| !path.exists())
            .map(|(ticket, _)| ticket.clone())
            .collect::<Vec<_>>();
        self.staged().retain(|_, path| path.exists());
        for ticket in missing {
            self.settle_managed(&ticket)
        }
        self.reconcile_missing();
        removed
    }

    fn stage(
        &self,
        ctx: &InvocationContext,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<ArtifactSource, ExportRefused> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(ExportRefused::HumanOnly);
        }
        if ctx.project_id().is_none() {
            return Err(ExportRefused::NoProject);
        }
        let io = |e: std::io::Error| ExportRefused::Io(e.to_string());
        std::fs::create_dir_all(&self.dir).map_err(io)?;
        let safe: String = file_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = self.dir.join(format!(
            "{}-{}-{safe}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        ));
        let attribution = if let Some(store) = &self.store {
            let project = ctx.project_id().cloned().ok_or(ExportRefused::NoProject)?;
            let id = self.sources.new_managed_file_id();
            let metadata = std::fs::symlink_metadata(&self.dir).map_err(io)?;
            let root = crate::fs::identity_at(&self.dir, &metadata)
                .ok_or_else(|| ExportRefused::Io("managed root identity unavailable".into()))?;
            let info = store.info();
            let reservation = ProjectManagedFileReservation {
                id: id.clone(),
                project_id: project.clone(),
                kind: "DiagnosticExport".into(),
                relative_path: path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .ok_or_else(|| ExportRefused::Io("managed file name unavailable".into()))?
                    .into(),
                root,
                created_identity: None,
                store_id: info.store_id,
                restore_epoch: info.restore_epoch,
                boot_id: info.boot_id,
                created_at_ms: self.sources.now_ms(),
            };
            let saved = reservation.clone();
            run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().reserve_project_managed_file(&saved)
            })
            .map_err(|_| ExportRefused::Io("export staging is closed or unavailable".into()))?;
            Some(reservation)
        } else {
            None
        };
        let mut attempt = StagingAttempt {
            owner: self,
            reservation: attribution.clone(),
            path: path.clone(),
            identity: None,
            committed: false,
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io)?;
        attempt.identity = crate::fs::identity_for_file(&file);
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        if let (Some(store), Some(reserved)) = (&self.store, &attribution) {
            let owner = PurgeFileOwner::DiagnosticExport {
                file_id: reserved.id.clone(),
            };
            let witness = crate::fs::purge::capture_managed_file(
                &self.dir,
                &devworks_core::ports::project_purge::PurgeFileSelection {
                    project_id: reserved.project_id.clone(),
                    store_id: reserved.store_id.clone(),
                    restore_epoch: reserved.restore_epoch,
                    owners: Vec::new(),
                },
                &reserved.relative_path,
                owner,
                &mut || false,
            )
            .map_err(|_| ExportRefused::Io("export staging identity changed".into()))?;
            let published = StoredProjectManagedFile {
                id: reserved.id.clone(),
                project_id: reserved.project_id.clone(),
                kind: reserved.kind.clone(),
                relative_path: reserved.relative_path.clone(),
                witness,
                created_at_ms: reserved.created_at_ms,
            };
            if run_write(store.as_ref(), WriteOptions::default(), move |tx| {
                tx.projects().register_project_managed_file(&published)
            })
            .is_err()
            {
                return Err(ExportRefused::Io("export staging expired or closed".into()));
            }
        }
        let ticket = match self.sources.mint_upload(ctx, &path, EXPORT_TICKET_MS) {
            Ok(t) => t,
            Err(r) => {
                return Err(match r {
                    IssueRefusal::HumanOnly => ExportRefused::HumanOnly,
                    other => ExportRefused::Io(format!("{other:?}")),
                });
            }
        };
        if let Some(reserved) = attribution {
            self.managed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(
                    ticket.as_str().to_owned(),
                    (reserved.project_id, reserved.id),
                );
        }
        self.staged().insert(ticket.as_str().to_owned(), path);
        attempt.committed = true;
        Ok(ArtifactSource::Upload { ticket_id: ticket })
    }
}
