//! `ResourceService`: named resources with capacity, fenced leases, priority queues and their
//! reconciliation (RES-01..03), behind `resources.list`, `.acquire`, `.renew`, `.release`,
//! `.holders` and `.waiters`, registered through
//! [`crate::services::UseCases::with_resources`]. The native UI and agent tools reach the same
//! use cases through the gateway; other harness operations call [`ResourceService::authorize_dispatch`]
//! at dispatch, so ownership and capacity are enforced, not advisory.
//!
//! # Identity and scope (RES-02)
//!
//! A resource is addressed by its name (`resourceId`), compared ignoring ASCII case. The host
//! registers physically shared resources once for the machine ([`register_machine`], for
//! example [`desktop`]): every project resolves that name to the same canonical
//! `machine:<name>` resource. Any other name resolves to the invoking project's private
//! `project:<project>:<name>` resource, created by its first acquire (capacity default 1,
//! labels), as CodeGraph resources are. A workspace invocation (no project) addresses machine
//! resources only. A project-bound invocation never sees another project's resources
//! (`NotFound`).
//!
//! # Allocation (RES-01, RES-03)
//!
//! Everything runs in the command's unit of work on the sole write owner, so allocation is
//! atomic; a retried request key replays its receipt. An acquire first settles the resource
//! (expiry, transfer, promotion), then either answers the holder's open lease (CodeGraph: a
//! repeat by the same holder — its session, or the principal without one — answers its hold;
//! a waiter's repeat keeps its place for another [`WAITER_LINGER_MS`]) or queues a new lease
//! and promotes. Waiters are granted by priority, first come first served within a priority,
//! strictly in order (a large head request is not overtaken). Each grant takes the resource's
//! next fencing token; renew and release must present it, and only the principal that made the
//! lease may use it. A renewal never grants again: a lapsed, released or waiting lease is
//! refused `Conflict` (acquire again).
//!
//! # Physical transfer (RES-03)
//!
//! An expired lease does not prove its owner stopped. A **logical** lease past its expiry ends
//! (`Expired`). A **physical** lease past its expiry, or released while it may still be in
//! use, becomes `Uncertain`/`Releasing` and keeps its capacity until its owner is verified
//! quiescent: every owned process serving it is settled (reaped or never started) and no
//! dispatch admitted under it is still in flight in this process ([`DispatchTicket`]). Only
//! then is it `Reconciled` and the next waiter granted. Stale fences are refused at dispatch,
//! renew and release.
//!
//! # Reconciliation and history (RES-03)
//!
//! [`ResourceService::reconcile`] runs the same rules for boot recovery, sleep/wake, pause,
//! shutdown and periodic sweeps, and records one attributed `ResourcesReconciled` event per
//! pass together with a history event for every lease transition (`LeaseQueued`,
//! `LeaseGranted`, `LeaseRenewed`, `LeaseReleasing`, `LeaseReleased`, `LeaseExpired`,
//! `LeaseCancelled`, `LeaseUncertain`, `LeaseReconciled`, `LeaseOwnerUnreachable`,
//! `LeaseOwnerPaused`), which the UI's history reads.
//!
//! # Retained CodeGraph behavior
//!
//! `WorkController.Acquire`/`RenewHold`/`ReleaseHold`, the `acquire`, `renew_hold` and
//! `release` work tools and `WorkViewsController.Resources` map to `resources.acquire`, `.renew`,
//! `.release` and `.list`/`.holders`/`.waiters`. Kept: create by first acquire with capacity and
//! labels that must equal an existing resource's, purpose (at most 500 characters), priority
//! (low, medium, high, critical), FIFO within priority with a queue position, a waiter's place
//! kept while it polls, same-holder repeats, renew/release only by the identity that made the
//! hold, an ended hold answered unchanged on release, a 24-hour bound on a lease, holds that do
//! not lapse because a session went quiet (`ownerUnreachableSinceMs` instead) and release on
//! `sessions.end`. Changed: holds are enforced at dispatch and fenced; waiting does not block
//! the call (`wait_seconds` becomes the queued lease the caller polls); physical resources are
//! transferred only after reconciliation.
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, resources as ops};
use crate::ports::storage::{
    LeaseEndReason, LeasePhase, NewEvent, NewResource, OpenLeaseFilter, ProcessFilter,
    QueuePosition, ReadSnapshot, ResourceFilter, ResourceRead, ResourceScopeKind, StorageError,
    StoredLease, StoredResource, WriteTransaction, canonical_key,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::records::{
    LeaseEnd, LeaseRecord, LeaseState, ResourcePriority, ResourceRecord, ResourceScope,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Limit, Page, PositiveCounter};
use crate::protocol::{
    ActorId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, ResourceId, RetryAdvice,
    RevisionDimension, RootId,
};
use crate::services::cursor::CursorBinding;
use crate::services::sessions::{Failure, Refusal, counter, decided, query_fault, rev};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

/// The operations [`ResourceService`] is registered for.
pub const OPERATIONS: [&str; 6] = [
    "resources.list",
    "resources.acquire",
    "resources.renew",
    "resources.release",
    "resources.holders",
    "resources.waiters",
];
/// Capacity of a resource its first acquire creates without naming one.
pub const DEFAULT_CAPACITY: i64 = 1;
/// Longest lease (and renewal): 24 hours, as CodeGraph holds.
pub const MAX_LEASE_MS: i64 = 86_400_000;
/// How long a waiter keeps its place after its latest acquire (CodeGraph `WaiterLingerSeconds`).
pub const WAITER_LINGER_MS: i64 = 120_000;
pub const MAX_PURPOSE_CHARS: usize = 500;
pub const MAX_LABELS: usize = 20;
pub const MAX_LABEL_CHARS: usize = 64;
/// Most holder and waiter IDs one resource record lists (CodeGraph `MaxListed`); the
/// `resources.holders`/`.waiters` pages list the rest.
pub const LISTED: usize = 50;
/// The canonical name of the machine desktop (input and focus), capacity one.
pub const DESKTOP: &str = "desktop";
/// Reserved lease purpose minted only by the reviewed desktop takeover owner.
pub const DESKTOP_CONSENT_PURPOSE: &str = "desktop-consent:";
/// History aggregate of lease transitions.
pub const LEASE_AGGREGATE: &str = "resource_lease";
/// History aggregate of reconciliation passes.
pub const RECONCILE_AGGREGATE: &str = "resources";
const PAGE: u16 = 500;

// ---------------------------------------------------------------------------------------------
// Machine resources and dispatch-time enforcement (RES-02)

/// A physically shared resource the host registers for the whole machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineResource {
    pub name: String,
    pub capacity: i64,
    pub physical: bool,
    pub labels: Vec<String>,
}
/// The machine desktop: one input/focus owner at a time, transferred only after reconciliation.
pub fn desktop() -> MachineResource {
    MachineResource {
        name: DESKTOP.into(),
        capacity: 1,
        physical: true,
        labels: vec!["desktop-input".into()],
    }
}
/// The machine resource a harness-controlled operation must hold a lease on before dispatch.
/// Desktop input and taking desktop control need the desktop; reading the screen does not.
pub fn required_resource(operation: &str) -> Option<&'static str> {
    matches!(operation, "desktop.input" | "desktop.request_control").then_some(DESKTOP)
}

/// Registers (or, when its definition changed, redefines) a machine resource. Idempotent.
/// Host code runs it at startup through its own unit of work.
pub fn register_machine(
    tx: &mut dyn WriteTransaction,
    def: &MachineResource,
) -> Result<StoredResource, StorageError> {
    let name = def.name.to_ascii_lowercase();
    let labels = normalized_labels(&def.labels);
    let key = canonical_key(ResourceScopeKind::Machine, None, &name);
    match tx.resources().resource_by_key(&key)? {
        Some(r)
            if r.capacity == def.capacity && r.labels == labels && r.physical == def.physical =>
        {
            Ok(r)
        }
        Some(r) => {
            let host = tx.store().host_actor.clone();
            let updated = tx.resources().redefine_resource(
                &r.id,
                r.revision,
                def.capacity,
                &labels,
                def.physical,
            )?;
            resource_event(
                tx,
                &host,
                &updated,
                "ResourceRedefined",
                json!({"capacity": def.capacity, "physical": def.physical}),
            )?;
            Ok(updated)
        }
        None => {
            let host = tx.store().host_actor.clone();
            let id = tx.new_id(IdKind::Resource);
            let created = tx.resources().insert_resource(NewResource {
                id,
                name,
                scope: ResourceScopeKind::Machine,
                project_id: None,
                physical: def.physical,
                capacity: def.capacity,
                labels,
            })?;
            resource_event(
                tx,
                &host,
                &created,
                "ResourceRegistered",
                json!({"capacity": def.capacity, "physical": def.physical}),
            )?;
            Ok(created)
        }
    }
}

/// Dispatches admitted under each lease that may still be running in this process.
#[derive(Debug, Default)]
pub struct Dispatches(Mutex<HashMap<String, usize>>);
impl Dispatches {
    fn add(&self, lease: &Id) {
        *self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(lease.as_str().to_owned())
            .or_default() += 1;
    }
    fn remove(&self, lease: &Id) {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(lease.as_str()) {
            *n -= 1;
            if *n == 0 {
                map.remove(lease.as_str());
            }
        }
    }
    /// How many admitted dispatches of `lease` are still in flight.
    pub fn in_flight(&self, lease: &Id) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(lease.as_str())
            .copied()
            .unwrap_or(0)
    }
}

/// Proof that one dispatch was admitted under a current lease. While it lives, a physical lease
/// cannot be transferred; drop it when the dispatched effect has finished (or is known not to
/// have started).
#[derive(Debug)]
pub struct DispatchTicket {
    dispatches: Arc<Dispatches>,
    pub lease: Id,
    pub resource: Id,
    pub fence: i64,
}
impl Drop for DispatchTicket {
    fn drop(&mut self) {
        self.dispatches.remove(&self.lease);
    }
}

/// Why a dispatch was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchRefusal {
    /// The resource (or lease) is not visible to this invocation.
    NotFound,
    /// The lease belongs to another principal or holder.
    NotOwner,
    /// The lease is not granted under this fence any more (stale owner, released, waiting).
    LeaseLost,
    /// The lease is past its expiry.
    Expired,
}
impl DispatchRefusal {
    /// The wire code a refused dispatch reports.
    pub fn code(&self) -> DomainErrorCode {
        match self {
            Self::NotFound => DomainErrorCode::NotFound,
            Self::NotOwner => DomainErrorCode::PermissionDenied,
            Self::LeaseLost | Self::Expired => DomainErrorCode::LeaseLost,
        }
    }
}
/// A dispatch check failure: a refusal or a storage error.
#[derive(Clone, Debug)]
pub enum DispatchFailure {
    Refused(DispatchRefusal),
    Storage(StorageError),
}
impl From<StorageError> for DispatchFailure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// Why a reconciliation pass runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileCause {
    /// Startup: leases recorded by earlier boots.
    Boot,
    /// The machine woke from sleep: expiries that passed while asleep.
    Wake,
    /// Orderly shutdown, after owned processes were stopped.
    Shutdown,
    /// A work root was paused: its leases are retained and recorded.
    Pause(RootId),
    /// A periodic or on-demand sweep.
    Sweep,
}
impl ReconcileCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Boot => "Boot",
            Self::Wake => "Wake",
            Self::Shutdown => "Shutdown",
            Self::Pause(_) => "Pause",
            Self::Sweep => "Sweep",
        }
    }
}

/// What one pass changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub resources: usize,
    pub granted: usize,
    pub expired: usize,
    pub cancelled: usize,
    pub uncertain: usize,
    pub reconciled: usize,
    pub unreachable: usize,
    pub paused: usize,
}
impl ReconcileReport {
    fn changed(&self) -> bool {
        self.granted
            + self.expired
            + self.cancelled
            + self.uncertain
            + self.reconciled
            + self.unreachable
            + self.paused
            > 0
    }
}

// ---------------------------------------------------------------------------------------------
// The service

pub struct ResourceService {
    ids: Arc<dyn IdSource>,
    /// The store clock: queries read expiry against it (commands use the transaction time).
    clock: Arc<dyn Clock>,
    dispatches: Arc<Dispatches>,
}

impl ResourceService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            ids,
            clock,
            dispatches: Arc::new(Dispatches::default()),
        }
    }
    /// The in-flight dispatch registry shared with the process supervisor.
    pub fn dispatches(&self) -> Arc<Dispatches> {
        self.dispatches.clone()
    }

    /// Reserve one resource for a container incarnation inside its create transaction.
    /// A distinct holder key prevents two containers in the same agent session from
    /// accidentally sharing a lease. The caller rolls back the transaction savepoint
    /// if any requested resource is queued.
    pub(crate) fn reserve_container(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        container: &Id,
        name: &str,
        machine: bool,
    ) -> Result<Option<Id>, Failure> {
        let resource = if machine {
            let key = canonical_key(ResourceScopeKind::Machine, None, name);
            if tx
                .resources()
                .resource_by_key(&key)?
                .is_some_and(|existing| {
                    existing.capacity != 1 || !existing.physical || !existing.labels.is_empty()
                })
            {
                return Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "container_port_definition",
                    None,
                )
                .into());
            }
            Some(register_machine(
                tx,
                &MachineResource {
                    name: name.into(),
                    capacity: 1,
                    physical: true,
                    labels: Vec::new(),
                },
            )?)
        } else {
            resolve(tx.resources(), ctx, name)?
        };
        let input = ops::AcquireInput {
            resource_id: ResourceId(Id::new(name).map_err(|_| {
                Refusal::new(DomainErrorCode::InvalidArgument, "container_resource", None)
            })?),
            expected_revision: crate::services::sessions::rev(
                resource.as_ref().map_or(1, |resource| resource.revision),
            ),
            units: Limit::new(1).expect("one resource unit"),
            lease_ms: PositiveCounter(MAX_LEASE_MS.to_string()),
            purpose: Some(BoundedText(format!("container:{}", container.as_str()))),
            priority: None,
            capacity: None,
            labels: None,
        };
        let holder = ctx.clone().with_session(container.clone());
        let decision = self.acquire_inner(tx, &holder, &input, false)?;
        let lease = match decision {
            DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire { value },
            }
            | DomainDecision::NoChange {
                result: OperationResult::ResourcesAcquire { value },
            } => value,
            _ => return Err(StorageError::InvalidRecord("container lease decision".into()).into()),
        };
        Ok(matches!(lease.state, LeaseState::Held).then_some(lease.id))
    }

    /// Release the leases only after the exact owned container has been observed stopped.
    /// The container row and lease transitions commit together.
    pub(crate) fn release_container_leases(
        &self,
        tx: &mut dyn WriteTransaction,
        container: &crate::ports::storage::StoredContainer,
    ) -> Result<(), StorageError> {
        if !matches!(
            container.phase,
            crate::ports::storage::ContainerPhase::Stopped
                | crate::ports::storage::ContainerPhase::Failed
        ) {
            return Err(StorageError::InvalidRecord(
                "container is not verified stopped or failed before dispatch".into(),
            ));
        }
        for id in &container.resource_lease_ids {
            let mut lease = tx.resources().lease(id)?.ok_or_else(|| {
                StorageError::InvalidRecord("container resource lease missing".into())
            })?;
            if lease.owner != container.actor
                || lease.holder_key != format!("session:{}", container.id.as_str())
            {
                return Err(StorageError::InvalidRecord(
                    "container resource lease ownership changed".into(),
                ));
            }
            if lease.phase != LeasePhase::Held {
                continue;
            }
            let resource = tx
                .resources()
                .resource(&lease.resource_id)?
                .ok_or_else(|| StorageError::InvalidRecord("container resource missing".into()))?;
            if resource.physical && !quiesced(tx, Some(&self.dispatches), &lease)? {
                transition(
                    tx,
                    &container.actor,
                    &mut lease,
                    LeasePhase::Releasing,
                    "LeaseReleasing",
                    json!({"reason":"container shutdown still reconciling"}),
                )?;
            } else {
                end(
                    tx,
                    &container.actor,
                    &mut lease,
                    LeaseEndReason::Released,
                    "LeaseReleased",
                    json!({"containerId":container.id.as_str()}),
                )?;
            }
            let host = tx.store().host_actor.clone();
            promote(tx, &host, &resource, &mut ReconcileReport::default())?;
        }
        Ok(())
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// The dispatch-time check every harness-controlled operation that needs `resource` runs
    /// inside its own unit of work before its effect (RES-02): the lease must be the
    /// invocation's own, granted under exactly `fence`, unexpired, on that resource. The
    /// returned ticket keeps a physical lease from being transferred while the effect runs.
    pub fn authorize_dispatch(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        resource: &str,
        lease: &Id,
        fence: i64,
    ) -> Result<DispatchTicket, DispatchFailure> {
        // Registered before the checks: a reconciliation serialized after this unit of work
        // sees it, and a refused check drops it again.
        self.dispatches.add(lease);
        let mut ticket = DispatchTicket {
            dispatches: self.dispatches.clone(),
            lease: lease.clone(),
            resource: lease.clone(),
            fence,
        };
        let Some(res) = resolve(tx.resources(), ctx, resource)? else {
            return Err(DispatchFailure::Refused(DispatchRefusal::NotFound));
        };
        let host = tx.store().host_actor.clone();
        settle(
            tx,
            &host,
            &res,
            Some(&self.dispatches),
            &mut ReconcileReport::default(),
        )?;
        let Some(l) = tx.resources().lease(lease)? else {
            return Err(DispatchFailure::Refused(DispatchRefusal::NotFound));
        };
        if l.resource_id != res.id {
            return Err(DispatchFailure::Refused(DispatchRefusal::NotFound));
        }
        if l.owner != *ctx.principal() || l.holder_key != holder_key(ctx) {
            return Err(DispatchFailure::Refused(DispatchRefusal::NotOwner));
        }
        if l.phase != LeasePhase::Held || l.fence != fence {
            return Err(DispatchFailure::Refused(DispatchRefusal::LeaseLost));
        }
        if l.expires_at_ms <= tx.now_ms() {
            return Err(DispatchFailure::Refused(DispatchRefusal::Expired));
        }
        ticket.resource = res.id;
        Ok(ticket)
    }

    /// One reconciliation pass (RES-03), in the caller's unit of work, attributed to the host.
    pub fn reconcile(
        &self,
        tx: &mut dyn WriteTransaction,
        cause: ReconcileCause,
    ) -> Result<ReconcileReport, StorageError> {
        let host = tx.store().host_actor.clone();
        let boot = current_boot(tx)?;
        let now = tx.now_ms();
        let mut report = ReconcileReport::default();
        let filter = match &cause {
            ReconcileCause::Boot => OpenLeaseFilter::PriorBoots(boot.clone()),
            ReconcileCause::Pause(root) => OpenLeaseFilter::Root(root.clone()),
            _ => OpenLeaseFilter::All,
        };
        let mut touched = BTreeSet::new();
        for mut l in open_leases(tx, &filter)? {
            touched.insert(l.resource_id.as_str().to_owned());
            let Some(res) = tx.resources().resource(&l.resource_id)? else {
                continue;
            };
            match &cause {
                ReconcileCause::Boot => match l.phase {
                    LeasePhase::Waiting => {
                        end(
                            tx,
                            &host,
                            &mut l,
                            LeaseEndReason::Cancelled,
                            "LeaseCancelled",
                            json!({"cause": "Boot"}),
                        )?;
                        report.cancelled += 1;
                    }
                    LeasePhase::Held if res.physical => {
                        l.unreachable_since_ms.get_or_insert(now);
                        transition(
                            tx,
                            &host,
                            &mut l,
                            LeasePhase::Uncertain,
                            "LeaseUncertain",
                            json!({"cause": "Boot", "reason": "owner of an earlier boot"}),
                        )?;
                        report.uncertain += 1;
                    }
                    LeasePhase::Held if l.unreachable_since_ms.is_none() => {
                        l.unreachable_since_ms = Some(now);
                        transition(
                            tx,
                            &host,
                            &mut l,
                            LeasePhase::Held,
                            "LeaseOwnerUnreachable",
                            json!({"cause": "Boot"}),
                        )?;
                        report.unreachable += 1;
                    }
                    _ => {}
                },
                ReconcileCause::Shutdown
                    if res.physical && l.phase == LeasePhase::Held && l.boot_id == boot =>
                {
                    l.unreachable_since_ms.get_or_insert(now);
                    transition(
                        tx,
                        &host,
                        &mut l,
                        LeasePhase::Uncertain,
                        "LeaseUncertain",
                        json!({"cause": "Shutdown", "reason": "owner stopping"}),
                    )?;
                    report.uncertain += 1;
                }
                ReconcileCause::Pause(_) => {
                    let phase = l.phase;
                    transition(
                        tx,
                        &host,
                        &mut l,
                        phase,
                        "LeaseOwnerPaused",
                        json!({"cause": "Pause", "retained": true}),
                    )?;
                    report.paused += 1;
                }
                _ => {}
            }
        }
        for id in touched {
            let id =
                Id::new(id).map_err(|_| StorageError::InvalidRecord("lease resource".into()))?;
            if let Some(res) = tx.resources().resource(&id)? {
                settle(tx, &host, &res, Some(&self.dispatches), &mut report)?;
                report.resources += 1;
            }
        }
        if cause != ReconcileCause::Sweep || report.changed() {
            let id = EventId(tx.new_id(IdKind::Event));
            let root = match &cause {
                ReconcileCause::Pause(r) => Some(r.clone()),
                _ => None,
            };
            tx.history().append(NewEvent {
                id,
                actor: host,
                project_id: None,
                root_id: root,
                operation_id: None,
                aggregate_kind: RECONCILE_AGGREGATE.into(),
                aggregate_id: boot.as_str().to_owned(),
                aggregate_revision: 1,
                kind: "ResourcesReconciled".into(),
                payload: json!({
                    "cause": cause.as_str(),
                    "resources": report.resources,
                    "granted": report.granted,
                    "expired": report.expired,
                    "cancelled": report.cancelled,
                    "uncertain": report.uncertain,
                    "reconciled": report.reconciled,
                    "unreachable": report.unreachable,
                    "paused": report.paused,
                }),
            })?;
        }
        Ok(report)
    }

    /// Settles the resource of one lease (after a process serving it settled, for example), in
    /// the caller's unit of work. Returns the lease as it is after.
    pub fn reconcile_lease(
        &self,
        tx: &mut dyn WriteTransaction,
        lease: &Id,
    ) -> Result<Option<StoredLease>, StorageError> {
        let Some(l) = tx.resources().lease(lease)? else {
            return Ok(None);
        };
        if let Some(res) = tx.resources().resource(&l.resource_id)? {
            let host = tx.store().host_actor.clone();
            settle(
                tx,
                &host,
                &res,
                Some(&self.dispatches),
                &mut ReconcileReport::default(),
            )?;
        }
        tx.resources().lease(lease)
    }

    // -----------------------------------------------------------------------------------------
    // Commands

    pub(crate) fn acquire_desktop(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::AcquireInput,
    ) -> Result<DomainDecision, Failure> {
        if !i.resource_id.0.as_str().eq_ignore_ascii_case(DESKTOP)
            || !i
                .purpose
                .as_ref()
                .is_some_and(|p| p.0.starts_with(DESKTOP_CONSENT_PURPOSE))
        {
            return Err(invalid("desktop_consent_lease").into());
        }
        self.acquire_inner(tx, ctx, i, true)
    }

    fn acquire(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::AcquireInput,
    ) -> Result<DomainDecision, Failure> {
        self.acquire_inner(tx, ctx, i, false)
    }

    fn acquire_inner(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::AcquireInput,
        desktop_owner: bool,
    ) -> Result<DomainDecision, Failure> {
        if !desktop_owner
            && i.purpose
                .as_ref()
                .is_some_and(|p| p.0.starts_with(DESKTOP_CONSENT_PURPOSE))
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent_lease",
                None,
            )
            .into());
        }
        // An archived (or deleted) project allocates nothing, a shared resource included.
        if let Some(p) = ctx.project_id()
            && tx
                .runs()
                .project_lifecycle(p)?
                .is_some_and(|l| crate::services::runs::closed_to_work(&l))
        {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "archived_project",
                Some(p.0.clone()),
            )
            .into());
        }
        let lease_ms = lease_ms(&i.lease_ms.0)?;
        let purpose = match &i.purpose {
            Some(p) if p.0.chars().count() > MAX_PURPOSE_CHARS => {
                return Err(invalid("purpose").into());
            }
            p => p.as_ref().map(|p| p.0.clone()),
        };
        let labels = match &i.labels {
            Some(l)
                if l.0.len() > MAX_LABELS
                    || l.0.iter().any(|x| x.0.chars().count() > MAX_LABEL_CHARS) =>
            {
                return Err(invalid("labels").into());
            }
            l => l
                .as_ref()
                .map(|l| normalized_labels(&l.0.iter().map(|x| x.0.clone()).collect::<Vec<_>>())),
        };
        let name = i.resource_id.0.as_str().to_ascii_lowercase();
        let expected = i.expected_revision.get();
        let res = match resolve(tx.resources(), ctx, &name)? {
            Some(r) => r,
            None => {
                let Some(project) = ctx.project_id() else {
                    return Err(not_found("resource", &i.resource_id.0).into());
                };
                if expected != 1 {
                    return Err(not_found("resource", &i.resource_id.0).into());
                }
                let id = tx.new_id(IdKind::Resource);
                let capacity = i
                    .capacity
                    .as_ref()
                    .map_or(DEFAULT_CAPACITY, |c| i64::from(c.get()));
                let created = match tx.resources().insert_resource(NewResource {
                    id,
                    name: name.clone(),
                    scope: ResourceScopeKind::Project,
                    project_id: Some(project.clone()),
                    physical: false,
                    capacity,
                    labels: labels.clone().unwrap_or_default(),
                }) {
                    Ok(r) => r,
                    Err(StorageError::NotFound { .. }) => {
                        return Err(not_found("project", &project.0).into());
                    }
                    Err(e) => return Err(e.into()),
                };
                resource_event(
                    tx,
                    ctx.principal(),
                    &created,
                    "ResourceCreated",
                    json!({"capacity": capacity}),
                )?;
                created
            }
        };
        if expected != res.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Claim,
                "resource",
                res.id.clone(),
                i.expected_revision.clone(),
                rev(res.revision),
            )
            .into());
        }
        if i.capacity
            .as_ref()
            .is_some_and(|c| i64::from(c.get()) != res.capacity)
            || labels.as_ref().is_some_and(|l| *l != res.labels)
        {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "resource_definition",
                Some(res.id.clone()),
            )
            .into());
        }
        let units = i64::from(i.units.get());
        if units > res.capacity {
            return Err(invalid("units").into());
        }
        let host = tx.store().host_actor.clone();
        settle(
            tx,
            &host,
            &res,
            Some(&self.dispatches),
            &mut ReconcileReport::default(),
        )?;
        let now = tx.now_ms();
        let boot = current_boot(tx)?;
        let key = holder_key(ctx);
        if let Some(mut l) = tx.resources().holder_lease(&res.id, &key)? {
            // A repeat by the same holder answers its lease; a waiter's repeat keeps its place,
            // and a holder seen again from this boot is reachable again.
            let refresh = l.phase == LeasePhase::Waiting
                || (l.phase == LeasePhase::Held
                    && (l.boot_id != boot || l.unreachable_since_ms.is_some()));
            if !refresh {
                return Ok(DomainDecision::NoChange {
                    result: OperationResult::ResourcesAcquire {
                        value: lease_record(tx.resources(), &l, &res, now)?,
                    },
                });
            }
            let kind = if l.phase == LeasePhase::Waiting {
                l.expires_at_ms = now + WAITER_LINGER_MS;
                "LeaseQueuePlaceKept"
            } else {
                "LeaseOwnerReachable"
            };
            l.boot_id = boot;
            l.unreachable_since_ms = None;
            let phase = l.phase;
            transition(tx, ctx.principal(), &mut l, phase, kind, json!({}))?;
            return Ok(DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire {
                    value: lease_record(tx.resources(), &l, &res, now)?,
                },
            });
        }
        let ticket = tx.resources().next_queue_ticket(&res.id)?;
        let lease = StoredLease {
            id: tx.new_id(IdKind::Lease),
            resource_id: res.id.clone(),
            phase: LeasePhase::Waiting,
            end_reason: None,
            owner: ctx.principal().clone(),
            holder_key: key,
            session_id: ctx.session_id().cloned(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            boot_id: boot,
            purpose,
            priority: i.priority.as_ref().map_or(1, priority_rank),
            units,
            lease_ms,
            queue_seq: ticket,
            fence: 0,
            requested_at_ms: now,
            granted_at_ms: None,
            renewed_at_ms: None,
            expires_at_ms: now + WAITER_LINGER_MS,
            ended_at_ms: None,
            unreachable_since_ms: None,
            revision: 1,
        };
        tx.resources().insert_lease(&lease)?;
        lease_event(
            tx,
            ctx.principal(),
            &lease,
            "LeaseQueued",
            json!({"units": units, "priority": lease.priority}),
        )?;
        promote(tx, &host, &res, &mut ReconcileReport::default())?;
        let lease = tx
            .resources()
            .lease(&lease.id)?
            .ok_or_else(|| StorageError::InvalidRecord("lease vanished".into()))?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ResourcesAcquire {
                value: lease_record(tx.resources(), &lease, &res, now)?,
            },
        })
    }

    /// The caller's own visible lease (`NotFound` / `PermissionDenied`), after settling its
    /// resource.
    fn own_lease(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<(StoredLease, StoredResource), Failure> {
        let (l, res) = visible_lease(tx.resources(), ctx, id)?;
        if l.owner != *ctx.principal() {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "resource_lease",
                Some(l.id),
            )
            .into());
        }
        let host = tx.store().host_actor.clone();
        settle(
            tx,
            &host,
            &res,
            Some(&self.dispatches),
            &mut ReconcileReport::default(),
        )?;
        let l = tx
            .resources()
            .lease(id)?
            .ok_or_else(|| StorageError::InvalidRecord("lease vanished".into()))?;
        Ok((l, res))
    }

    fn renew(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::RenewInput,
    ) -> Result<DomainDecision, Failure> {
        let lease_ms = lease_ms(&i.lease_ms.0)?;
        let (mut l, res) = self.own_lease(tx, ctx, &i.lease_id)?;
        if l.purpose
            .as_deref()
            .is_some_and(|p| p.starts_with(DESKTOP_CONSENT_PURPOSE))
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "desktop_consent_lease",
                Some(l.id),
            )
            .into());
        }
        if l.fence != i.expected_fence.get() {
            return Err(lease_lost(&l));
        }
        // An archived (or deleted) project's work is stopping: its holds are not extended (they
        // are released, or lapse; a physical one transfers only after its trees quiesce).
        if project_closed(tx, l.project_id.as_ref())? {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "archived_project",
                l.project_id.as_ref().map(|p| p.0.clone()),
            )
            .into());
        }
        if l.phase != LeasePhase::Held {
            // A renewal never grants again: a lapsed, released or waiting lease is acquired anew.
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "resource_lease",
                Some(l.id.clone()),
            )
            .into());
        }
        let now = tx.now_ms();
        l.expires_at_ms = now + lease_ms;
        l.lease_ms = lease_ms;
        l.renewed_at_ms = Some(now);
        l.boot_id = current_boot(tx)?;
        l.unreachable_since_ms = None;
        let payload = json!({"expiresAtMs": l.expires_at_ms});
        transition(
            tx,
            ctx.principal(),
            &mut l,
            LeasePhase::Held,
            "LeaseRenewed",
            payload,
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ResourcesRenew {
                value: lease_record(tx.resources(), &l, &res, now)?,
            },
        })
    }

    pub(crate) fn release(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ReleaseInput,
    ) -> Result<DomainDecision, Failure> {
        let (mut l, res) = self.own_lease(tx, ctx, &i.lease_id)?;
        if l.fence != i.expected_fence.get() {
            return Err(lease_lost(&l));
        }
        let now = tx.now_ms();
        let unchanged =
            |tx: &mut dyn WriteTransaction, l: &StoredLease| -> Result<DomainDecision, Failure> {
                Ok(DomainDecision::NoChange {
                    result: OperationResult::ResourcesRelease {
                        value: lease_record(tx.resources(), l, &res, now)?,
                    },
                })
            };
        match l.phase {
            // Releasing an ended lease changes nothing; one already releasing transfers once
            // its owner is verified stopped.
            LeasePhase::Released | LeasePhase::Releasing | LeasePhase::Uncertain => {
                return unchanged(tx, &l);
            }
            LeasePhase::Waiting => {
                end(
                    tx,
                    ctx.principal(),
                    &mut l,
                    LeaseEndReason::Cancelled,
                    "LeaseCancelled",
                    json!({"cause": "Released"}),
                )?;
            }
            LeasePhase::Held if res.physical && !quiesced(tx, Some(&self.dispatches), &l)? => {
                transition(
                    tx,
                    ctx.principal(),
                    &mut l,
                    LeasePhase::Releasing,
                    "LeaseReleasing",
                    json!({"reason": "owned work still running"}),
                )?;
            }
            LeasePhase::Held => {
                end(
                    tx,
                    ctx.principal(),
                    &mut l,
                    LeaseEndReason::Released,
                    "LeaseReleased",
                    json!({}),
                )?;
            }
        }
        let host = tx.store().host_actor.clone();
        promote(tx, &host, &res, &mut ReconcileReport::default())?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ResourcesRelease {
                value: lease_record(tx.resources(), &l, &res, now)?,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Queries

    fn list(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ListInput,
        now: i64,
    ) -> Result<OperationResult, Failure> {
        let filter = match (&i.scope, ctx.project_id()) {
            (DataScope::Workspace, None) => ResourceFilter::All,
            (DataScope::Workspace, Some(_)) => {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "workspace_scope",
                    None,
                )
                .into());
            }
            (DataScope::Project { project_id }, own) => {
                if own.is_some_and(|p| p != project_id)
                    || !read.sessions().project_exists(project_id)?
                {
                    return Err(not_found("project", &project_id.0).into());
                }
                ResourceFilter::Project(project_id.clone())
            }
        };
        let scope_part = match &filter {
            ResourceFilter::All => String::from("*"),
            ResourceFilter::Project(p) => p.0.as_str().to_owned(),
        };
        let binding = CursorBinding::new(ctx, "resources.list", &[&scope_part], 1);
        let after = i
            .page
            .cursor
            .as_ref()
            .map(|c| binding.decode(&c.0))
            .transpose()?;
        let limit = i.page.limit.get() as usize;
        // A full page probes for one more row separately: the store bounds a listing at
        // `MAX_PAGE`, which is also the largest wire page, so it cannot over-fetch by one.
        let rows = read
            .resources()
            .resources(&filter, after.as_deref(), limit as u16)?;
        let has_more = rows.len() == limit
            && !read
                .resources()
                .resources(&filter, Some(&rows[limit - 1].canonical_key), 1)?
                .is_empty();
        let next = has_more.then(|| BoundedText(binding.encode(&rows[limit - 1].canonical_key)));
        let items = rows
            .iter()
            .map(|r| resource_record(read.resources(), r, now))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OperationResult::ResourcesList {
            value: Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
        })
    }

    fn holders(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::HoldersInput,
        now: i64,
    ) -> Result<OperationResult, Failure> {
        let res = resolve(read.resources(), ctx, i.resource_id.0.as_str())?
            .ok_or_else(|| not_found("resource", &i.resource_id.0))?;
        let binding = CursorBinding::new(
            ctx,
            "resources.holders",
            &[&res.canonical_key],
            res.revision,
        );
        let mut after = match &i.page.cursor {
            None => 0,
            Some(c) => binding
                .decode(&c.0)?
                .parse::<i64>()
                .map_err(|_| StorageError::CursorExpired)?,
        };
        let limit = i.page.limit.get() as usize;
        let mut items = Vec::new();
        let mut has_more = false;
        'pages: loop {
            let page = read.resources().holders(&res.id, after, PAGE)?;
            let full = page.len() == PAGE as usize;
            for l in page {
                after = l.fence;
                if effective(&l, &res, now).0 == LeasePhase::Released {
                    continue;
                }
                if items.len() == limit {
                    has_more = true;
                    break 'pages;
                }
                items.push(l);
            }
            if !full {
                break;
            }
        }
        let next = has_more
            .then(|| BoundedText(binding.encode(&items.last().map_or(0, |l| l.fence).to_string())));
        let items = items
            .iter()
            .map(|l| lease_record(read.resources(), l, &res, now))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(OperationResult::ResourcesHolders {
            value: Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
        })
    }

    fn waiters(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::WaitersInput,
        now: i64,
    ) -> Result<OperationResult, Failure> {
        let res = resolve(read.resources(), ctx, i.resource_id.0.as_str())?
            .ok_or_else(|| not_found("resource", &i.resource_id.0))?;
        let binding = CursorBinding::new(
            ctx,
            "resources.waiters",
            &[&res.canonical_key],
            res.revision,
        );
        // The cursor carries the last listed (priority, ticket) and how many live waiters came
        // before it, so positions stay exact across pages.
        let (mut after, mut position) = match &i.page.cursor {
            None => (None, 0),
            Some(c) => {
                let p = binding.decode(&c.0)?;
                let v: Vec<i64> = p
                    .split(':')
                    .map(str::parse)
                    .collect::<Result<_, _>>()
                    .map_err(|_| StorageError::CursorExpired)?;
                match v[..] {
                    [priority, queue_seq, position] => (
                        Some(QueuePosition {
                            priority,
                            queue_seq,
                        }),
                        position,
                    ),
                    _ => return Err(StorageError::CursorExpired.into()),
                }
            }
        };
        let limit = i.page.limit.get() as usize;
        let mut items = Vec::new();
        let mut has_more = false;
        'pages: loop {
            let page = read.resources().waiters(&res.id, after, PAGE)?;
            let full = page.len() == PAGE as usize;
            for l in page {
                if effective(&l, &res, now).0 == LeasePhase::Released {
                    after = Some(QueuePosition {
                        priority: l.priority,
                        queue_seq: l.queue_seq,
                    });
                    continue;
                }
                if items.len() == limit {
                    has_more = true;
                    break 'pages;
                }
                after = Some(QueuePosition {
                    priority: l.priority,
                    queue_seq: l.queue_seq,
                });
                position += 1;
                items.push((l, position));
            }
            if !full {
                break;
            }
        }
        let next = has_more.then(|| {
            let last = &items[items.len() - 1].0;
            BoundedText(binding.encode(&format!("{}:{}:{position}", last.priority, last.queue_seq)))
        });
        let items = items
            .iter()
            .map(|(l, p)| {
                let mut r = lease_record_at(l, &res, now, None);
                r.queue_position = Some(counter(*p));
                r
            })
            .collect();
        Ok(OperationResult::ResourcesWaiters {
            value: Page {
                items: BoundedList(items),
                next_cursor: next,
                has_more,
            },
        })
    }
}

impl QueryHandler for ResourceService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let OperationInput::Resources(input) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(self.correlation()),
            );
        };
        let now = self.clock.now_ms().get();
        let answered = match input {
            ops::Input::List(i) => self.list(snapshot, ctx, i, now),
            ops::Input::Holders(i) => self.holders(snapshot, ctx, i, now),
            ops::Input::Waiters(i) => self.waiters(snapshot, ctx, i, now),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let result = answered.map_err(|f| query_fault(f, "resource", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl CommandHandler for ResourceService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Resources(ops::Input::Acquire(i)) => self.acquire(tx, ctx, i),
            OperationInput::Resources(ops::Input::Renew(i)) => self.renew(tx, ctx, i),
            OperationInput::Resources(ops::Input::Release(i)) => self.release(tx, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        decided(tx, outcome)
    }
}

/// Whether `project` is archived or deleted (closed to work).
fn project_closed(
    tx: &mut dyn WriteTransaction,
    project: Option<&crate::protocol::ProjectId>,
) -> Result<bool, StorageError> {
    let Some(p) = project else {
        return Ok(false);
    };
    Ok(tx
        .runs()
        .project_lifecycle(p)?
        .is_some_and(|l| crate::services::runs::closed_to_work(&l)))
}

/// Cancels the waiting leases of a project being archived, in the archival's unit of work, and
/// promotes the next waiters of each resource they left (other projects' work). Held leases are
/// left to their stopping owners (released, lapsing, or transferred after quiescence).
pub fn cancel_project_waiters(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    project: &crate::protocol::ProjectId,
) -> Result<usize, StorageError> {
    let mut n = 0;
    let mut touched = BTreeSet::new();
    for mut l in open_leases(tx, &OpenLeaseFilter::Project(project.clone()))? {
        if l.phase != LeasePhase::Waiting {
            continue;
        }
        end(
            tx,
            actor,
            &mut l,
            LeaseEndReason::Cancelled,
            "LeaseCancelled",
            json!({"cause": "ProjectArchived"}),
        )?;
        n += 1;
        touched.insert(l.resource_id.as_str().to_owned());
    }
    for id in touched {
        let id = Id::new(id).map_err(|_| StorageError::InvalidRecord("lease resource".into()))?;
        if let Some(res) = tx.resources().resource(&id)? {
            promote(tx, actor, &res, &mut ReconcileReport::default())?;
        }
    }
    Ok(n)
}

/// Ends the open leases of a session that just ended (CodeGraph: `end_session` frees its
/// holds), in that unit of work. A physical lease still possibly in use becomes `Releasing` and
/// transfers at the next reconciliation.
pub fn end_session_leases(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    session: &Id,
) -> Result<usize, StorageError> {
    let mut n = 0;
    let mut touched = BTreeSet::new();
    for mut l in open_leases(tx, &OpenLeaseFilter::Session(session.clone()))? {
        let Some(res) = tx.resources().resource(&l.resource_id)? else {
            continue;
        };
        match l.phase {
            LeasePhase::Waiting => {
                end(
                    tx,
                    actor,
                    &mut l,
                    LeaseEndReason::Cancelled,
                    "LeaseCancelled",
                    json!({"cause": "SessionEnded"}),
                )?;
            }
            LeasePhase::Held if res.physical && !quiesced(tx, None, &l)? => {
                transition(
                    tx,
                    actor,
                    &mut l,
                    LeasePhase::Releasing,
                    "LeaseReleasing",
                    json!({"cause": "SessionEnded"}),
                )?;
            }
            LeasePhase::Held => {
                end(
                    tx,
                    actor,
                    &mut l,
                    LeaseEndReason::Released,
                    "LeaseReleased",
                    json!({"cause": "SessionEnded"}),
                )?;
            }
            _ => continue,
        }
        n += 1;
        touched.insert(res.id.as_str().to_owned());
    }
    for id in touched {
        let id = Id::new(id).map_err(|_| StorageError::InvalidRecord("lease resource".into()))?;
        if let Some(res) = tx.resources().resource(&id)? {
            promote(tx, actor, &res, &mut ReconcileReport::default())?;
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// Allocation rules

fn current_boot(tx: &dyn WriteTransaction) -> Result<Id, StorageError> {
    tx.store()
        .boot_id
        .clone()
        .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))
}
fn holder_key(ctx: &InvocationContext) -> String {
    match ctx.session_id() {
        Some(s) => format!("session:{}", s.as_str()),
        None => format!("actor:{}", ctx.principal().0.as_str()),
    }
}
fn normalized_labels(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
fn priority_rank(p: &ResourcePriority) -> i64 {
    match p {
        ResourcePriority::Low => 0,
        ResourcePriority::Medium => 1,
        ResourcePriority::High => 2,
        ResourcePriority::Critical => 3,
    }
}
fn priority_of(rank: i64) -> ResourcePriority {
    match rank {
        0 => ResourcePriority::Low,
        2 => ResourcePriority::High,
        3 => ResourcePriority::Critical,
        _ => ResourcePriority::Medium,
    }
}
fn invalid(field: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, field, None)
}
fn not_found(kind: &'static str, id: &Id) -> Refusal {
    Refusal::new(DomainErrorCode::NotFound, kind, Some(id.clone()))
}
fn lease_lost(l: &StoredLease) -> Failure {
    Refusal::new(
        DomainErrorCode::LeaseLost,
        "resource_lease",
        Some(l.id.clone()),
    )
    .retry(RetryAdvice::RefreshAndNewRequest)
    .into()
}
fn lease_ms(s: &str) -> Result<i64, Refusal> {
    match s.parse::<i64>() {
        Ok(ms) if ms > 0 && ms <= MAX_LEASE_MS => Ok(ms),
        _ => Err(invalid("leaseMs")),
    }
}

/// The resource `name` resolves to for this invocation: the machine resource of that name, else
/// the invoking project's private one.
fn resolve(
    read: &dyn ResourceRead,
    ctx: &InvocationContext,
    name: &str,
) -> Result<Option<StoredResource>, StorageError> {
    let name = name.to_ascii_lowercase();
    if let Some(r) =
        read.resource_by_key(&canonical_key(ResourceScopeKind::Machine, None, &name))?
    {
        return Ok(Some(r));
    }
    match ctx.project_id() {
        Some(p) => read.resource_by_key(&canonical_key(ResourceScopeKind::Project, Some(p), &name)),
        None => Ok(None),
    }
}
fn visible(ctx: &InvocationContext, res: &StoredResource) -> bool {
    match (&res.project_id, ctx.project_id()) {
        (None, _) | (_, None) => true,
        (Some(owner), Some(p)) => owner == p,
    }
}
fn visible_lease(
    read: &dyn ResourceRead,
    ctx: &InvocationContext,
    id: &Id,
) -> Result<(StoredLease, StoredResource), Failure> {
    let l = read
        .lease(id)?
        .ok_or_else(|| not_found("resource_lease", id))?;
    match read.resource(&l.resource_id)? {
        Some(res) if visible(ctx, &res) => Ok((l, res)),
        _ => Err(not_found("resource_lease", id).into()),
    }
}

fn open_leases(
    tx: &mut dyn WriteTransaction,
    filter: &OpenLeaseFilter,
) -> Result<Vec<StoredLease>, StorageError> {
    let mut out = Vec::new();
    let mut after: Option<Id> = None;
    loop {
        let page = tx.resources().open_leases(filter, after.as_ref(), PAGE)?;
        let full = page.len() == PAGE as usize;
        after = page.last().map(|l| l.id.clone());
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}

/// Whether the owner of a lease is verified quiescent: every owned process serving it is
/// settled, and no dispatch admitted under it is in flight in this process. Without the
/// registry, only a lease of an earlier boot can be (its in-process effects ended with it).
fn quiesced(
    tx: &mut dyn WriteTransaction,
    dispatches: Option<&Dispatches>,
    l: &StoredLease,
) -> Result<bool, StorageError> {
    if tx.resources().container_lease_in_use(&l.id)? {
        return Ok(false);
    }
    let mut after: Option<Id> = None;
    loop {
        let page =
            tx.resources()
                .processes(&ProcessFilter::Lease(l.id.clone()), after.as_ref(), PAGE)?;
        if page.iter().any(|p| !p.phase.quiesced()) {
            return Ok(false);
        }
        if page.len() < PAGE as usize {
            break;
        }
        after = page.last().map(|p| p.id.clone());
    }
    let this_boot = tx.store().boot_id.as_ref() == Some(&l.boot_id);
    Ok(match dispatches {
        Some(d) => d.in_flight(&l.id) == 0,
        None => !this_boot,
    })
}

/// Expiry, transfer and promotion for one resource at the transaction time.
fn settle(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    res: &StoredResource,
    dispatches: Option<&Dispatches>,
    report: &mut ReconcileReport,
) -> Result<(), StorageError> {
    let now = tx.now_ms();
    let mut after = 0;
    loop {
        let page = tx.resources().holders(&res.id, after, PAGE)?;
        let full = page.len() == PAGE as usize;
        for mut l in page {
            after = l.fence;
            let expired = l.expires_at_ms <= now;
            if l.phase == LeasePhase::Held && expired {
                if res.physical {
                    l.unreachable_since_ms.get_or_insert(l.expires_at_ms);
                    transition(
                        tx,
                        actor,
                        &mut l,
                        LeasePhase::Uncertain,
                        "LeaseUncertain",
                        json!({"reason": "expired while its owner may still be running"}),
                    )?;
                    report.uncertain += 1;
                } else {
                    end(
                        tx,
                        actor,
                        &mut l,
                        LeaseEndReason::Expired,
                        "LeaseExpired",
                        json!({}),
                    )?;
                    report.expired += 1;
                    continue;
                }
            }
            if matches!(l.phase, LeasePhase::Releasing | LeasePhase::Uncertain)
                && (!res.physical || quiesced(tx, dispatches, &l)?)
            {
                let reason = if l.phase == LeasePhase::Releasing {
                    LeaseEndReason::Released
                } else {
                    LeaseEndReason::Reconciled
                };
                end(
                    tx,
                    actor,
                    &mut l,
                    reason,
                    "LeaseReconciled",
                    json!({"verified": "owner quiescent"}),
                )?;
                report.reconciled += 1;
            }
        }
        if !full {
            break;
        }
    }
    let mut after = None;
    loop {
        let page = tx.resources().waiters(&res.id, after, PAGE)?;
        let full = page.len() == PAGE as usize;
        for mut l in page {
            after = Some(QueuePosition {
                priority: l.priority,
                queue_seq: l.queue_seq,
            });
            if l.expires_at_ms <= now {
                end(
                    tx,
                    actor,
                    &mut l,
                    LeaseEndReason::Expired,
                    "LeaseExpired",
                    json!({"waiting": true}),
                )?;
                report.expired += 1;
            }
        }
        if !full {
            break;
        }
    }
    promote(tx, actor, res, report)
}

/// Grants waiters in priority/FIFO order while the head fits the free capacity. A waiter past its
/// linger deadline was abandoned: it is expired here, whichever caller promotes, never granted.
fn promote(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    res: &StoredResource,
    report: &mut ReconcileReport,
) -> Result<(), StorageError> {
    let now = tx.now_ms();
    let mut used = 0;
    let mut after = 0;
    loop {
        let page = tx.resources().holders(&res.id, after, PAGE)?;
        let full = page.len() == PAGE as usize;
        for l in &page {
            used += l.units;
            after = l.fence;
        }
        if !full {
            break;
        }
    }
    loop {
        let Some(mut head) = tx.resources().waiters(&res.id, None, 1)?.pop() else {
            return Ok(());
        };
        if head.expires_at_ms <= now {
            end(
                tx,
                actor,
                &mut head,
                LeaseEndReason::Expired,
                "LeaseExpired",
                json!({"waiting": true}),
            )?;
            report.expired += 1;
            continue;
        }
        // A waiter of an archived (or deleted) project is never granted, by any path that
        // promotes (a release, housekeeping, a transfer): it leaves the queue.
        if project_closed(tx, head.project_id.as_ref())? {
            end(
                tx,
                actor,
                &mut head,
                LeaseEndReason::Cancelled,
                "LeaseCancelled",
                json!({"cause": "ProjectClosed", "waiting": true}),
            )?;
            continue;
        }
        if head.units > res.capacity - used {
            return Ok(());
        }
        head.fence = tx.resources().next_fence(&res.id)?;
        head.granted_at_ms = Some(now);
        head.expires_at_ms = now + head.lease_ms;
        used += head.units;
        let payload = json!({"fence": head.fence, "units": head.units});
        transition(
            tx,
            actor,
            &mut head,
            LeasePhase::Held,
            "LeaseGranted",
            payload,
        )?;
        report.granted += 1;
    }
}

fn transition(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    l: &mut StoredLease,
    to: LeasePhase,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let expected = l.revision;
    l.phase = to;
    l.revision += 1;
    tx.resources().update_lease(l, expected)?;
    lease_event(tx, actor, l, kind, payload)
}
fn end(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    l: &mut StoredLease,
    reason: LeaseEndReason,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    l.end_reason = Some(reason);
    l.ended_at_ms = Some(tx.now_ms());
    transition(tx, actor, l, LeasePhase::Released, kind, payload)
}
fn lease_event(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    l: &StoredLease,
    kind: &str,
    mut payload: Value,
) -> Result<(), StorageError> {
    if let Value::Object(m) = &mut payload {
        m.insert("resourceId".into(), json!(l.resource_id.as_str()));
        m.insert("state".into(), json!(l.phase.as_str()));
        m.insert("fence".into(), json!(l.fence));
        if let Some(e) = l.end_reason {
            m.insert("endReason".into(), json!(e.as_str()));
        }
        if let Some(p) = &l.purpose {
            m.insert("purpose".into(), json!(p));
        }
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: l.project_id.clone(),
        root_id: l.root_id.clone(),
        operation_id: None,
        aggregate_kind: LEASE_AGGREGATE.into(),
        aggregate_id: l.id.as_str().to_owned(),
        aggregate_revision: l.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
fn resource_event(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    r: &StoredResource,
    kind: &str,
    mut payload: Value,
) -> Result<(), StorageError> {
    if let Value::Object(m) = &mut payload {
        m.insert("canonicalKey".into(), json!(r.canonical_key));
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: r.project_id.clone(),
        root_id: None,
        operation_id: None,
        aggregate_kind: "resource".into(),
        aggregate_id: r.id.as_str().to_owned(),
        aggregate_revision: r.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Wire records

/// The lease as it reads at `now`: a lapsed logical grant or waiter has ended, a lapsed physical
/// grant is uncertain (its owner may still be running).
fn effective(
    l: &StoredLease,
    res: &StoredResource,
    now: i64,
) -> (LeasePhase, Option<LeaseEndReason>) {
    match l.phase {
        LeasePhase::Held if l.expires_at_ms <= now && res.physical => (LeasePhase::Uncertain, None),
        LeasePhase::Held | LeasePhase::Waiting if l.expires_at_ms <= now => {
            (LeasePhase::Released, Some(LeaseEndReason::Expired))
        }
        p => (p, l.end_reason),
    }
}
fn wire_state(p: LeasePhase) -> LeaseState {
    match p {
        LeasePhase::Waiting => LeaseState::Waiting,
        LeasePhase::Held => LeaseState::Held,
        LeasePhase::Releasing => LeaseState::Releasing,
        LeasePhase::Uncertain => LeaseState::Uncertain,
        LeasePhase::Released => LeaseState::Released,
    }
}
fn wire_end(e: LeaseEndReason) -> LeaseEnd {
    match e {
        LeaseEndReason::Released => LeaseEnd::Released,
        LeaseEndReason::Expired => LeaseEnd::Expired,
        LeaseEndReason::Cancelled => LeaseEnd::Cancelled,
        LeaseEndReason::Reconciled => LeaseEnd::Reconciled,
    }
}
fn resource_id_of(res: &StoredResource) -> ResourceId {
    // Names are validated opaque identifiers when stored.
    ResourceId(Id::new(res.name.clone()).unwrap_or_else(|_| res.id.clone()))
}
fn lease_record_at(
    l: &StoredLease,
    res: &StoredResource,
    now: i64,
    queue_position: Option<i64>,
) -> LeaseRecord {
    let (phase, end_reason) = effective(l, res, now);
    let lapsed = phase != l.phase;
    LeaseRecord {
        id: l.id.clone(),
        resource_id: resource_id_of(res),
        owner_id: l.owner.clone(),
        fence: counter(l.fence),
        expires_at_ms: counter(l.expires_at_ms),
        state: wire_state(phase),
        units: Limit::new(l.units as u32),
        purpose: l.purpose.clone().map(BoundedText),
        priority: Some(priority_of(l.priority)),
        queue_position: queue_position.map(counter),
        requested_at_ms: Some(counter(l.requested_at_ms)),
        granted_at_ms: l.granted_at_ms.map(counter),
        renewed_at_ms: l.renewed_at_ms.map(counter),
        ended_at_ms: match (l.ended_at_ms, lapsed && phase == LeasePhase::Released) {
            (Some(t), _) => Some(counter(t)),
            (None, true) => Some(counter(l.expires_at_ms)),
            (None, false) => None,
        },
        end_reason: end_reason.map(wire_end),
        owner_unreachable_since_ms: l
            .unreachable_since_ms
            .or((lapsed && phase == LeasePhase::Uncertain).then_some(l.expires_at_ms))
            .map(counter),
        project_id: l.project_id.clone(),
        root_id: l.root_id.clone(),
        assignment_id: l.assignment_id.clone(),
        session_id: l.session_id.clone(),
    }
}
/// The record of one lease, with its queue position while it waits.
fn lease_record(
    read: &dyn ResourceRead,
    l: &StoredLease,
    res: &StoredResource,
    now: i64,
) -> Result<LeaseRecord, StorageError> {
    let position = if l.phase == LeasePhase::Waiting {
        let mut after = None;
        let mut n = 0;
        let mut found = None;
        'pages: loop {
            let page = read.waiters(&res.id, after, PAGE)?;
            let full = page.len() == PAGE as usize;
            for w in &page {
                after = Some(QueuePosition {
                    priority: w.priority,
                    queue_seq: w.queue_seq,
                });
                if effective(w, res, now).0 == LeasePhase::Released {
                    continue;
                }
                n += 1;
                if w.id == l.id {
                    found = Some(n);
                    break 'pages;
                }
            }
            if !full {
                break;
            }
        }
        found
    } else {
        None
    };
    Ok(lease_record_at(l, res, now, position))
}
fn resource_record(
    read: &dyn ResourceRead,
    res: &StoredResource,
    now: i64,
) -> Result<ResourceRecord, StorageError> {
    let mut holders = Vec::new();
    let mut used = 0;
    let mut after = 0;
    loop {
        let page = read.holders(&res.id, after, PAGE)?;
        let full = page.len() == PAGE as usize;
        for l in &page {
            after = l.fence;
            if effective(l, res, now).0 == LeasePhase::Released {
                continue;
            }
            used += l.units;
            if holders.len() < LISTED {
                holders.push(l.id.clone());
            }
        }
        if !full {
            break;
        }
    }
    let waiters = read
        .waiters(&res.id, None, PAGE)?
        .into_iter()
        .filter(|l| effective(l, res, now).0 != LeasePhase::Released)
        .take(LISTED)
        .map(|l| l.id)
        .collect();
    Ok(ResourceRecord {
        id: resource_id_of(res),
        canonical_key: BoundedText(res.canonical_key.clone()),
        revision: rev(res.revision),
        capacity: Limit::new(res.capacity as u32).unwrap_or_else(|| Limit::new(1).expect("1")),
        holders: BoundedList(holders),
        waiters: BoundedList(waiters),
        scope: Some(match res.scope {
            ResourceScopeKind::Machine => ResourceScope::Machine,
            ResourceScopeKind::Project => ResourceScope::Project,
        }),
        project_id: res.project_id.clone(),
        physical: Some(res.physical),
        labels: Some(BoundedList(
            res.labels.iter().cloned().map(BoundedText).collect(),
        )),
        used: Some(counter(used)),
        available: Some(used < res.capacity),
    })
}
