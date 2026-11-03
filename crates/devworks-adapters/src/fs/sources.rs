//! The production [`ArtifactSources`]: host-issued artifact source handles.
//!
//! A handle names a file the host already decided may become an artifact; a caller only ever
//! presents its opaque ID. Every handle is bound at issue time, and it resolves only for exactly
//! that binding (anything else is refused, existence not disclosed to other principals):
//!
//! * **Upload ticket** — minted by [`HostArtifactSources::mint_upload`] at user ingress for a
//!   file the user picked (the trusted local input path); bound to that user, the invocation's
//!   project (required) and work root, and an expiry. Expired tickets are evicted. An agent, package, MCP client or schedule can never
//!   mint one.
//! * **Attachment** — registered by the host for the imported attachment record that owns it
//!   ([`HostArtifactSources::register_attachment`]), bound to that record's project. The
//!   importer of existing CodeGraph work (imports) owns those records and calls this seam.
//! * **Work output** — registered by the host for a file in a managed worktree
//!   ([`HostArtifactSources::register_work_output`]), bound to the producing principal, project
//!   and work root, with the repository revision as provenance. The worktree/process service
//!   that owns those records calls this seam.
//!
//! Resolution returns the registered read root (with the identity recorded when the handle was
//! issued) and relative path; the artifact service then opens the file through the verified
//! no-follow reader, so a root replaced or a component swapped for a link since is refused.
//! Each kind has its own namespace; registering an ID twice is refused, never overwritten.
//! Handles live in process memory: they are short-lived and never survive a restart.
use super::identity_of;
use devworks_core::authority::reads::ReadRoot;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::files::{ArtifactSources, ResolvedSource, SourceOrigin, SourceRefusal};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::{ActorId, Id, ProjectId, RootId};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Longest lifetime of an upload ticket.
pub const MAX_UPLOAD_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Upload,
    Attachment,
    WorkOutput,
}

#[derive(Clone, Debug)]
struct Handle {
    root: ReadRoot,
    relative: String,
    principal: Option<ActorId>,
    project: Option<ProjectId>,
    work_root: Option<RootId>,
    expires_at_ms: Option<i64>,
    origin: SourceOrigin,
}

/// Why a handle could not be issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueRefusal {
    /// Upload tickets are minted only at user ingress.
    HumanOnly,
    /// Attachment and work-output handles are registered only by the host.
    HostOnly,
    /// The picked path is not a regular file (directories, links and devices are refused).
    NotAFile,
    /// A handle with this ID is already registered for its kind (never overwritten).
    Duplicate,
    Invalid(&'static str),
}

/// Host-issued source handles (see the module docs).
pub struct HostArtifactSources {
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdSource>,
    /// One map per kind: an ID registered as one kind never resolves as another.
    handles: Mutex<HashMap<Kind, HashMap<String, Handle>>>,
}

/// A read root for `dir` with the identity it has now (Unix); Windows reads use held
/// no-delete-share handles and register no identity.
fn root_of(dir: &Path) -> Result<ReadRoot, IssueRefusal> {
    let meta =
        std::fs::symlink_metadata(dir).map_err(|_| IssueRefusal::Invalid("no such directory"))?;
    if !meta.is_dir() {
        return Err(IssueRefusal::Invalid("not a directory"));
    }
    Ok(ReadRoot {
        location: dir.to_path_buf(),
        identity: identity_of(&meta),
    })
}

impl HostArtifactSources {
    pub fn new(clock: Arc<dyn Clock>, ids: Arc<dyn IdSource>) -> Self {
        Self {
            clock,
            ids,
            handles: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a handle, refusing a duplicate ID of the same kind, and evicts expired ones.
    fn insert(&self, kind: Kind, id: &Id, handle: Handle) -> Result<(), IssueRefusal> {
        let now = self.clock.now_ms().get();
        let mut all = self.handles.lock().unwrap_or_else(|p| p.into_inner());
        let map = all.entry(kind).or_default();
        map.retain(|_, h| h.expires_at_ms.is_none_or(|t| t > now));
        if map.contains_key(id.as_str()) {
            return Err(IssueRefusal::Duplicate);
        }
        map.insert(id.as_str().to_owned(), handle);
        Ok(())
    }

    /// Mints an upload ticket for `file`, which the user picked through the app's own file
    /// dialog. User ingress only; bound to the user, the invocation's project and work root,
    /// and `lifetime_ms` (1 ms to 24 hours).
    pub fn mint_upload(
        &self,
        ctx: &InvocationContext,
        file: &Path,
        lifetime_ms: i64,
    ) -> Result<Id, IssueRefusal> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(IssueRefusal::HumanOnly);
        }
        let Some(project) = ctx.project_id().cloned() else {
            return Err(IssueRefusal::Invalid(
                "an upload ticket is bound to a project",
            ));
        };
        if !(1..=MAX_UPLOAD_LIFETIME_MS).contains(&lifetime_ms) {
            return Err(IssueRefusal::Invalid("ticket lifetime out of bounds"));
        }
        let meta = std::fs::symlink_metadata(file).map_err(|_| IssueRefusal::NotAFile)?;
        if !meta.file_type().is_file() {
            return Err(IssueRefusal::NotAFile);
        }
        let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str()))
        else {
            return Err(IssueRefusal::Invalid("not a file path"));
        };
        let id = self.ids.next_id(IdKind::SourceHandle);
        self.insert(
            Kind::Upload,
            &id,
            Handle {
                root: root_of(dir)?,
                relative: name.to_owned(),
                principal: Some(ctx.principal().clone()),
                project: Some(project),
                work_root: ctx.root_id().cloned(),
                expires_at_ms: Some(self.clock.now_ms().get().saturating_add(lifetime_ms)),
                origin: SourceOrigin {
                    original_name: Some(name.to_owned()),
                    ..SourceOrigin::default()
                },
            },
        )?;
        Ok(id)
    }

    /// Registers the stored file of an imported attachment record (host only), bound to the
    /// record's project. `relative` is beneath `root`.
    pub fn register_attachment(
        &self,
        ctx: &InvocationContext,
        attachment_id: &Id,
        project: &ProjectId,
        root: &Path,
        relative: &str,
        original_name: Option<String>,
    ) -> Result<(), IssueRefusal> {
        if ctx.principal_kind() != PrincipalKind::Host {
            return Err(IssueRefusal::HostOnly);
        }
        self.insert(
            Kind::Attachment,
            attachment_id,
            Handle {
                root: root_of(root)?,
                relative: relative.to_owned(),
                principal: None,
                project: Some(project.clone()),
                work_root: None,
                expires_at_ms: None,
                origin: SourceOrigin {
                    original_name,
                    ..SourceOrigin::default()
                },
            },
        )
    }

    /// Registers a file an assignment produced in its managed worktree (host only), bound to
    /// the producing principal, project and work root. `origin` carries the repository and
    /// revision.
    #[allow(clippy::too_many_arguments)]
    pub fn register_work_output(
        &self,
        ctx: &InvocationContext,
        handle: &Id,
        producer: &ActorId,
        project: &ProjectId,
        work_root: Option<RootId>,
        worktree: &Path,
        relative: &str,
        origin: SourceOrigin,
    ) -> Result<(), IssueRefusal> {
        if ctx.principal_kind() != PrincipalKind::Host {
            return Err(IssueRefusal::HostOnly);
        }
        self.insert(
            Kind::WorkOutput,
            handle,
            Handle {
                root: root_of(worktree)?,
                relative: relative.to_owned(),
                principal: Some(producer.clone()),
                project: Some(project.clone()),
                work_root,
                expires_at_ms: None,
                origin,
            },
        )
    }

    /// Withdraws a handle of every kind with this ID (its owning record went away).
    pub fn revoke(&self, handle: &Id) {
        for map in self
            .handles
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values_mut()
        {
            map.remove(handle.as_str());
        }
    }
}

impl ArtifactSources for HostArtifactSources {
    fn resolve(
        &self,
        ctx: &InvocationContext,
        source: &ArtifactSource,
        project: &ProjectId,
    ) -> Result<ResolvedSource, SourceRefusal> {
        let (kind, id) = match source {
            ArtifactSource::Upload { ticket_id } => (Kind::Upload, ticket_id),
            ArtifactSource::Attachment { attachment_id } => (Kind::Attachment, attachment_id),
            ArtifactSource::WorkOutput { file_handle } => (Kind::WorkOutput, file_handle),
        };
        let now = self.clock.now_ms().get();
        let handle = {
            let mut all = self.handles.lock().unwrap_or_else(|p| p.into_inner());
            let map = all.entry(kind).or_default();
            map.retain(|_, h| h.expires_at_ms.is_none_or(|t| t > now));
            map.get(id.as_str()).cloned()
        }
        .ok_or(SourceRefusal::NotFound)?;
        // Another principal's handle is unknown to this caller.
        if handle
            .principal
            .as_ref()
            .is_some_and(|p| p != ctx.principal())
        {
            return Err(SourceRefusal::NotFound);
        }
        // A binding is satisfied only by exactly the bound project and work root: an invocation
        // without a root does not satisfy a root-bound handle.
        let foreign_project = handle.project.as_ref().is_some_and(|p| p != project)
            || ctx.project_id().is_some_and(|p| p != project);
        let foreign_root = handle
            .work_root
            .as_ref()
            .is_some_and(|r| ctx.root_id() != Some(r));
        if foreign_project || foreign_root {
            return Err(SourceRefusal::PermissionDenied);
        }
        Ok(ResolvedSource {
            root: handle.root,
            relative: handle.relative,
            origin: handle.origin,
        })
    }
}
