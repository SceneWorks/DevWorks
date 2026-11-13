//! Resource reservation and owned-process records (RES-01..03, the process owner).
//!
//! * **Resources** have a canonical key: `machine:<name>` for a physically shared resource the
//!   host registered (one machine desktop, a GPU, a port range), shared by every project, or
//!   `project:<project>:<name>` for a project-private resource created by its first acquire.
//!   Names are compared ignoring ASCII case (CodeGraph resource quirk). The row revision is the
//!   resource's *definition* revision (capacity, labels, physical), not its occupancy.
//! * **Leases** are one owner's use of `units` of a resource: queued (`Waiting`), granted
//!   (`Held`), released while owned processes are still being stopped (`Releasing`), expired or
//!   orphaned while its holder may still be running (`Uncertain`), or ended (`Released`, with
//!   an end reason). A grant takes the resource's next fencing token; a lease that was never
//!   granted has fence 0.
//! * **Owned processes** record the native identity (pid plus a birth token, never the pid
//!   alone), the process group or job, the boot that launched them, their attribution and the
//!   lease they serve, so recovery can reconcile them after a crash.
//!
//! The repository enforces record invariants (identity, one open lease per holder and resource,
//! unique fences, compare-and-set revisions, bounded listings). Allocation order, fencing,
//! expiry, transfer and reconciliation rules belong to `crate::services::resources`, which runs
//! inside one unit of work.
use super::StorageError;
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};

/// Whether a resource is shared by the machine or private to one project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceScopeKind {
    Machine,
    Project,
}

/// A stored resource definition with its allocation counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResource {
    pub id: Id,
    /// The resource name (case-folded), as callers address it.
    pub name: String,
    pub canonical_key: String,
    pub scope: ResourceScopeKind,
    /// The owning project of a project-private resource; `None` for a machine resource.
    pub project_id: Option<ProjectId>,
    /// A physical resource is transferred only after its previous owner is verified stopped.
    pub physical: bool,
    pub capacity: i64,
    pub labels: Vec<String>,
    /// Definition revision (positive).
    pub revision: i64,
    /// Last fencing token issued for a grant of this resource.
    pub fence_seq: i64,
    /// Last queue ticket issued (FIFO order within a priority).
    pub queue_seq: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A new resource definition (revision 1, counters 0).
#[derive(Clone, Debug)]
pub struct NewResource {
    pub id: Id,
    pub name: String,
    pub scope: ResourceScopeKind,
    pub project_id: Option<ProjectId>,
    pub physical: bool,
    pub capacity: i64,
    pub labels: Vec<String>,
}
impl NewResource {
    /// The canonical key this definition is stored under.
    pub fn canonical_key(&self) -> String {
        canonical_key(self.scope, self.project_id.as_ref(), &self.name)
    }
}
/// The canonical key of `name` in a scope.
pub fn canonical_key(scope: ResourceScopeKind, project: Option<&ProjectId>, name: &str) -> String {
    match (scope, project) {
        (ResourceScopeKind::Machine, _) | (ResourceScopeKind::Project, None) => {
            format!("machine:{}", name.to_ascii_lowercase())
        }
        (ResourceScopeKind::Project, Some(p)) => {
            format!("project:{}:{}", p.0.as_str(), name.to_ascii_lowercase())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LeasePhase {
    Waiting,
    Held,
    Releasing,
    Uncertain,
    Released,
}
impl LeasePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Held => "Held",
            Self::Releasing => "Releasing",
            Self::Uncertain => "Uncertain",
            Self::Released => "Released",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Waiting" => Self::Waiting,
            "Held" => Self::Held,
            "Releasing" => Self::Releasing,
            "Uncertain" => Self::Uncertain,
            "Released" => Self::Released,
            _ => return None,
        })
    }
    /// Whether the lease occupies capacity (granted and not yet transferable).
    pub fn occupies(self) -> bool {
        matches!(self, Self::Held | Self::Releasing | Self::Uncertain)
    }
}

/// Why a lease ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LeaseEndReason {
    /// Its owner released it.
    Released,
    /// Its time to live ran out (a logical resource, or a waiter that stopped polling).
    Expired,
    /// It left the queue before a grant, or its owning session ended while it waited.
    Cancelled,
    /// A physical lease whose previous owner was verified stopped by reconciliation.
    Reconciled,
}
impl LeaseEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Released => "Released",
            Self::Expired => "Expired",
            Self::Cancelled => "Cancelled",
            Self::Reconciled => "Reconciled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Released" => Self::Released,
            "Expired" => Self::Expired,
            "Cancelled" => Self::Cancelled,
            "Reconciled" => Self::Reconciled,
            _ => return None,
        })
    }
}

/// One lease with its ownership, lifecycle and fencing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLease {
    pub id: Id,
    pub resource_id: Id,
    pub phase: LeasePhase,
    pub end_reason: Option<LeaseEndReason>,
    pub owner: ActorId,
    /// Same-holder identity: the owning session, or the principal without one.
    pub holder_key: String,
    pub session_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    /// The boot that last acted for this lease (its owner's process lifetime).
    pub boot_id: Id,
    pub purpose: Option<String>,
    /// 0 Low, 1 Medium, 2 High, 3 Critical.
    pub priority: i64,
    pub units: i64,
    /// Requested time to live of a grant (milliseconds).
    pub lease_ms: i64,
    pub queue_seq: i64,
    /// Fencing token of the grant; 0 while waiting.
    pub fence: i64,
    pub requested_at_ms: i64,
    pub granted_at_ms: Option<i64>,
    pub renewed_at_ms: Option<i64>,
    /// Grant expiry, or the waiter's keep-place deadline while waiting.
    pub expires_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub unreachable_since_ms: Option<i64>,
    pub revision: i64,
}

/// Where a waiter listing resumes: after this (priority, queue ticket).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuePosition {
    pub priority: i64,
    pub queue_seq: i64,
}

/// Which resources a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResourceFilter {
    /// Machine resources plus the private resources of one project.
    Project(ProjectId),
    /// Every resource (a workspace view).
    All,
}

/// Which open (not `Released`) leases a sweep covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenLeaseFilter {
    /// Every open lease.
    All,
    /// Open leases recorded by a boot other than this one.
    PriorBoots(Id),
    /// Open leases of one owning session.
    Session(Id),
    /// Open leases of one work root.
    Root(RootId),
    /// Open leases of one project.
    Project(ProjectId),
}

// ---------------------------------------------------------------------------------------------
// Owned processes

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcessPhase {
    /// Recorded before the spawn; no native identity yet.
    Launching,
    Running,
    /// Cancellation was requested and the tree is being quiesced or reaped.
    Stopping,
    /// The whole tree was verified gone.
    Reaped,
    /// The tree could not be verified stopped (deadline, escaped descendant, lost handle).
    Uncertain,
    /// The spawn failed; nothing ran.
    Failed,
    /// A launch of an earlier boot recorded no identity: whether anything runs is unknown.
    Lost,
}
impl ProcessPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "Launching",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Reaped => "Reaped",
            Self::Uncertain => "Uncertain",
            Self::Failed => "Failed",
            Self::Lost => "Lost",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "Launching" => Self::Launching,
            "Running" => Self::Running,
            "Stopping" => Self::Stopping,
            "Reaped" => Self::Reaped,
            "Uncertain" => Self::Uncertain,
            "Failed" => Self::Failed,
            "Lost" => Self::Lost,
            _ => return None,
        })
    }
    /// Verified to run nothing (a lease served by it may be transferred).
    pub fn quiesced(self) -> bool {
        matches!(self, Self::Reaped | Self::Failed)
    }
}

/// One owned process tree as recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProcess {
    pub id: Id,
    pub boot_id: Id,
    pub phase: ProcessPhase,
    /// Native identity: pid, birth token (start time) and process group or job identity.
    pub pid: Option<i64>,
    pub birth: Option<String>,
    pub group: Option<i64>,
    /// `unix` or `windows`.
    pub platform: String,
    /// The program name (redacted, without its directory) and the digest of the full argv.
    pub program: String,
    pub argv_sha256: String,
    pub operation_id: Option<OperationId>,
    pub lease_id: Option<Id>,
    pub actor: ActorId,
    pub session_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    pub exit_code: Option<i64>,
    /// The last observation (redacted text), for example why a stop is uncertain.
    pub detail: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub revision: i64,
}

/// A process launch intent, recorded before the spawn.
#[derive(Clone, Debug)]
pub struct NewProcess {
    pub id: Id,
    pub platform: String,
    pub program: String,
    pub argv_sha256: String,
    pub operation_id: Option<OperationId>,
    pub lease_id: Option<Id>,
    pub actor: ActorId,
    pub session_id: Option<Id>,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
}

/// Which owned processes a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessFilter {
    /// Every owned process launched for this exact journaled operation, including settled ones.
    Operation(OperationId),
    /// Processes serving one lease.
    Lease(Id),
    /// Processes not yet quiesced (`Launching`, `Running`, `Stopping`, `Uncertain`, `Lost`)
    /// recorded by a boot other than this one.
    UnsettledPriorBoots(Id),
    /// Processes not yet quiesced, of any boot.
    Unsettled,
    /// Processes not yet quiesced, of any boot, recorded under this root.
    UnsettledRoot(RootId),
}

/// Durable ownership and observed lifecycle of a container created for agent work. The public
/// ID is also the exact engine name; the backend label and this record must agree before any
/// stop, so a pre-existing or reused name cannot acquire app ownership by accident.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredContainer {
    pub id: Id,
    pub engine: String,
    pub phase: ContainerPhase,
    pub image: String,
    pub actor: ActorId,
    pub owner_kind: String,
    pub session_id: Option<Id>,
    pub project_id: ProjectId,
    pub root_id: Option<RootId>,
    pub assignment_id: Option<AssignmentId>,
    pub create_operation_id: OperationId,
    pub last_operation_id: OperationId,
    /// Fenced resource leases held for this exact external container incarnation.
    pub resource_lease_ids: Vec<Id>,
    pub detail: Option<String>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerPhase {
    Creating,
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    Uncertain,
}
impl ContainerPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "Creating",
            Self::Created => "Created",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Stopped => "Stopped",
            Self::Failed => "Failed",
            Self::Uncertain => "Uncertain",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Creating" => Self::Creating,
            "Created" => Self::Created,
            "Starting" => Self::Starting,
            "Running" => Self::Running,
            "Stopping" => Self::Stopping,
            "Stopped" => Self::Stopped,
            "Failed" => Self::Failed,
            "Uncertain" => Self::Uncertain,
            _ => return None,
        })
    }
    pub fn may_run(self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Starting | Self::Running | Self::Stopping | Self::Uncertain
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerFilter {
    /// All agent-work records whose engine process may still be running, including uncertain.
    AgentWorkMayRun,
    /// The agent-work records of one root whose engine process may still be running.
    AgentWorkMayRunForRoot(RootId),
    Project(ProjectId),
}

pub trait ResourceRead {
    fn resource(&self, id: &Id) -> Result<Option<StoredResource>, StorageError>;
    fn resource_by_key(&self, canonical_key: &str) -> Result<Option<StoredResource>, StorageError>;
    /// Resources in canonical-key order strictly after `after`, at most `limit` (1..=500).
    fn resources(
        &self,
        filter: &ResourceFilter,
        after: Option<&str>,
        limit: u16,
    ) -> Result<Vec<StoredResource>, StorageError>;
    fn lease(&self, id: &Id) -> Result<Option<StoredLease>, StorageError>;
    /// The open lease of `holder_key` on a resource, if any.
    fn holder_lease(
        &self,
        resource: &Id,
        holder_key: &str,
    ) -> Result<Option<StoredLease>, StorageError>;
    /// Leases occupying the resource (`Held`, `Releasing`, `Uncertain`) in grant (fence) order
    /// strictly after fence `after`, at most `limit` (1..=500).
    fn holders(
        &self,
        resource: &Id,
        after: i64,
        limit: u16,
    ) -> Result<Vec<StoredLease>, StorageError>;
    /// Waiting leases by priority (highest first), then queue ticket (oldest first), strictly
    /// after `after`, at most `limit` (1..=500).
    fn waiters(
        &self,
        resource: &Id,
        after: Option<QueuePosition>,
        limit: u16,
    ) -> Result<Vec<StoredLease>, StorageError>;
    /// Open leases matching `filter` in lease-ID order strictly after `after`, at most `limit`.
    fn open_leases(
        &self,
        filter: &OpenLeaseFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredLease>, StorageError>;
    fn process(&self, id: &Id) -> Result<Option<StoredProcess>, StorageError>;
    /// Owned processes matching `filter` in ID order strictly after `after`, at most `limit`.
    fn processes(
        &self,
        filter: &ProcessFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredProcess>, StorageError>;
    fn container(&self, id: &Id) -> Result<Option<StoredContainer>, StorageError>;
    /// A physical lease cannot transfer while a linked external container may still run.
    fn container_lease_in_use(&self, lease: &Id) -> Result<bool, StorageError>;
    /// Container records in ID order strictly after `after`, at most `limit`.
    fn containers(
        &self,
        filter: &ContainerFilter,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredContainer>, StorageError>;
}

pub trait ResourceRepository: ResourceRead {
    /// Inserts a definition at revision 1. An existing canonical key is
    /// [`StorageError::Duplicate`]; an unknown project is [`StorageError::NotFound`].
    fn insert_resource(&mut self, resource: NewResource) -> Result<StoredResource, StorageError>;
    /// Replaces the definition (capacity, labels, physical) when the stored revision is
    /// `expected`, advancing it; otherwise a `Claim` [`StorageError::RevisionConflict`].
    fn redefine_resource(
        &mut self,
        id: &Id,
        expected: i64,
        capacity: i64,
        labels: &[String],
        physical: bool,
    ) -> Result<StoredResource, StorageError>;
    /// Issues the resource's next fencing token.
    fn next_fence(&mut self, resource: &Id) -> Result<i64, StorageError>;
    /// Issues the resource's next queue ticket.
    fn next_queue_ticket(&mut self, resource: &Id) -> Result<i64, StorageError>;
    /// Inserts a new lease (revision 1). A second open lease of the same holder on the same
    /// resource, or a reused fence, is [`StorageError::Duplicate`].
    fn insert_lease(&mut self, lease: &StoredLease) -> Result<(), StorageError>;
    /// Replaces a lease whose stored revision is `expected` (the new value carries
    /// `expected + 1`); a different stored revision is [`StorageError::LeaseLost`].
    fn update_lease(&mut self, lease: &StoredLease, expected: i64) -> Result<(), StorageError>;
    /// Records a launch intent (`Launching`, revision 1) for this store's boot.
    fn insert_process(&mut self, process: NewProcess) -> Result<StoredProcess, StorageError>;
    /// Replaces a process record whose stored revision is `expected` (the new value carries
    /// `expected + 1`); a different stored revision is [`StorageError::LeaseLost`].
    fn update_process(
        &mut self,
        process: &StoredProcess,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Record the ownership intent before invoking a backend create command.
    fn insert_container(&mut self, container: &StoredContainer) -> Result<(), StorageError>;
    /// Compare-and-set a lifecycle transition; its revision must be `expected + 1`.
    fn update_container(
        &mut self,
        container: &StoredContainer,
        expected: i64,
    ) -> Result<(), StorageError>;
}
