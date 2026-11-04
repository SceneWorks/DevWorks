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
use devworks_core::protocol::data::ArtifactSource;
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
}

impl DiagnosticExports {
    /// `dir`: a host-owned directory (normally `<data root>/diagnostics/exports`).
    pub fn new(dir: impl Into<PathBuf>, sources: Arc<HostArtifactSources>) -> Self {
        Self {
            dir: dir.into(),
            sources,
            next: AtomicU64::new(0),
            staged: Mutex::new(HashMap::new()),
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
        self.staged().retain(|_, path| path.exists());
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
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        let ticket = match self.sources.mint_upload(ctx, &path, EXPORT_TICKET_MS) {
            Ok(t) => t,
            Err(r) => {
                let _ = std::fs::remove_file(&path);
                return Err(match r {
                    IssueRefusal::HumanOnly => ExportRefused::HumanOnly,
                    other => ExportRefused::Io(format!("{other:?}")),
                });
            }
        };
        self.staged().insert(ticket.as_str().to_owned(), path);
        Ok(ArtifactSource::Upload { ticket_id: ticket })
    }
}
