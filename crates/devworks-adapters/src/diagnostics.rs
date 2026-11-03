//! The production [`ExportStaging`] for diagnostic exports: the rendered, redacted export is
//! written to a host-owned directory (under the data root's `diagnostics/exports`) and minted
//! as an upload ticket of the exporting person ([`HostArtifactSources::mint_upload`]), which the
//! export's `artifacts.save` then resolves like any file the user picked. The ticket is bound to
//! that person, the invocation's project and work root, and expires; an agent, package, MCP
//! client or schedule cannot mint one. Staged files are removed once older than the ticket
//! lifetime ([`DiagnosticExports::sweep`]); the artifact keeps its own copy.
use crate::fs::sources::{HostArtifactSources, IssueRefusal};
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::diagnostics::{ExportRefused, ExportStaging};
use devworks_core::protocol::data::ArtifactSource;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Lifetime of an export's upload ticket.
pub const EXPORT_TICKET_MS: i64 = 10 * 60 * 1000;

/// Stages diagnostic exports as upload tickets (see the module documentation).
pub struct DiagnosticExports {
    dir: PathBuf,
    sources: Arc<HostArtifactSources>,
    next: AtomicU64,
}

impl DiagnosticExports {
    /// `dir`: a host-owned directory (normally `<data root>/diagnostics/exports`).
    pub fn new(dir: impl Into<PathBuf>, sources: Arc<HostArtifactSources>) -> Self {
        Self {
            dir: dir.into(),
            sources,
            next: AtomicU64::new(0),
        }
    }

    /// Removes staged export files older than the ticket lifetime; returns how many.
    pub fn sweep(&self) -> usize {
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
        removed
    }
}

impl ExportStaging for DiagnosticExports {
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
        let ticket = self
            .sources
            .mint_upload(ctx, &path, EXPORT_TICKET_MS)
            .map_err(|r| match r {
                IssueRefusal::HumanOnly => ExportRefused::HumanOnly,
                other => ExportRefused::Io(format!("{other:?}")),
            })?;
        Ok(ArtifactSource::Upload { ticket_id: ticket })
    }
}
