//! `PackageService`: managed executable packages (language packs and harness packages) behind
//! `addons.catalog`, `.install`, `.configure`, `.enable`, `.disable`, `.update`, `.propose`,
//! `.logs`, `.rollback` and `.uninstall` (ADDON-01..04, EXT-01..07), registered through
//! [`crate::services::UseCases::with_packages`]. The Settings Add-ons screen and agent tools reach
//! the same use cases through the gateway; the package host (`devworks-engine::package_host`)
//! uses only the host seams below ([`PackageService::retry_pending`],
//! [`PackageService::startup_failed`], [`PackageService::record_running`],
//! [`PackageService::collect_files`]) and never writes the registry another way.
//!
//! Available packs from signed publisher indexes and the durable announcement of availability
//! and active-version changes are in [`index`]; configuration values are validated against the
//! package's shipped schema document through the [`SchemaValidator`] port.
//!
//! # Installation (architecture "Manifest installation and compatibility")
//!
//! A bundle is an immutable artifact. [`CommandHandler::prepare`] (outside any write) reads
//! exactly the referenced version, has the [`PackageInspector`] verify it — container, archive
//! paths (traversal, absolute, aliasing, duplicates), the strict manifest, every file's digest
//! and length, and an Ed25519 signature when present (an invalid signature is refused, never
//! treated as unsigned) — and publishes the verified files into a content-addressed install
//! directory with [`PackageFiles::publish`] (stage, flush, verify, rename). Nothing of the
//! package executes during installation; there are no installer scripts. An interrupted
//! publication never becomes visible: only a committed version record names a directory, and
//! unreferenced directories are collected before admission at boot and by age afterwards.
//!
//! `decide` then records the immutable version (identity = manifest digest; reusing a version
//! string with other contents is another version) and either activates it or stages it with the
//! waits it must clear, in the command's one unit of work.
//!
//! # Approval and trust (EXT-02, EXT-07)
//!
//! * A **person** installing or updating a bundle approves exactly those contents (the digest
//!   they reviewed is the one recorded). A pending proposal for those contents is approved in the
//!   same unit of work.
//! * **Agent-authored** contents — a proposal (`addons.propose`) exists for the manifest digest,
//!   or the bundle artifact was produced by an agent — always wait for a person's
//!   `ExactVersionApproval`, even when signed by a trusted publisher. An agent can never approve:
//!   its install/update of such contents stays staged and the active version is unchanged.
//! * Any other non-person install of new contents waits for `TrustApproval` (signed) or
//!   `ExactVersionApproval` (unsigned local development content).
//! * After a person explicitly trusts the active version's publisher for updates
//!   (`addons.configure` with the host update-policy schema [`UPDATE_POLICY_SCHEMA`]; the receipt
//!   binds publisher, signing key, package, the stable stream and the approved access envelope),
//!   a compatible signed update activates automatically ([`policy::auto_update`]): a newer stable
//!   version of the same major, signed by the same key, with no access beyond the approved
//!   envelope, no new dependency or runtime, an unchanged state schema and unchanged public
//!   contribution contracts. Expanded access waits for `AccessApproval`; a key change for
//!   `TrustApproval`; any other transition for `ExactVersionApproval`.
//! * A version a person once approved (or that once activated) keeps that approval: rollback to
//!   it, or reinstalling it, needs no new approval.
//!
//! No approvals service exists yet (DW-14 owns human decisions). Proposals are stored here with
//! their decision; the approval of a proposal is the person's own `addons.install`/`.update` of
//! exactly its contents, and [`PackageService::decide_proposal`] is the seam DW-14's
//! `approvals.record_human_decision` will call (approve or reject, person only). An approval
//! that leaves nothing else to wait for makes the pending version activatable at the host's
//! next quiescent boundary. Only a person's decision rejects a version: a newer proposal and an
//! uninstall retire the candidate they replace (its files go, its identity stays), so the same
//! contents can be installed again through the ordinary approval path.
//!
//! # Publication (architecture "Durable package state and atomic activation")
//!
//! The registry is one revisioned head (`ActivePackageSet` revision, the commands'
//! `expectedRegistryRevision`) plus one record per package (desired enablement, the active
//! pointer, the rollback predecessor, the pending version, the state generation, configuration
//! and trust). An activation commits, in one transaction: the pointer, a new state generation
//! (the predecessor generation is retained), the version's approval, the registry head, history
//! and a diagnostic. The committed registry *is* the activation intent: the package host
//! reconciles its processes to it after every commit and at boot, so a crash before the commit
//! keeps the old pointer and one after it is completed by the host.
//!
//! **Quiescence.** An update or rollback of a package whose helper is running first gates its new
//! invocations through the host's barrier ([`PackageActivity::quiesce`]) and waits for in-flight
//! invocations to finish (bounded by [`PackageService::quiesce_wait`]); only a quiescent package
//! is published, so no invocation ever runs against a half-published registry, and an invocation
//! already admitted keeps its pinned version. A package that is still busy keeps the candidate
//! staged (`Quiescence`) and the host retries ([`PackageService::retry_pending`]) with backoff,
//! rechecking approval, trust and compatibility in the activation's unit of work (a candidate
//! that would regress now stays pending for `Compatibility` and is retried); routine updates
//! never interrupt work. Compatibility is evaluated with the candidate as the active version
//! ([`policy::activation_regresses`]): its own prerequisites and every package that would stop
//! running (reverse pins). An explicit activation retires an obsolete pending version that
//! waited only for the host; one waiting on a person's decision stays pending.
//!
//! **State.** Each activation publishes a new state generation, so a handler pinned to the old
//! generation is fenced (`broker.commit_event` refuses a generation mismatch). A version with the
//! same state schema carries the state forward; a changed schema also seeds the new generation
//! with the carried-forward state (the package migrates its own namespace) while the predecessor
//! generation stays retained for rollback.
//!
//! **Rollback** (`addons.rollback`, guarded by the expected registry revision and state
//! generation) activates a retained installed version. When its state schema equals the current
//! one, the current state (with every later write) is carried forward; otherwise rollback is
//! allowed only while the current generation has not been written since it was published, and
//! then restores exactly the predecessor generation it was seeded from. Any other downgrade is
//! refused (`Conflict`, `package_state`): rollback never restores an old snapshot over later work,
//! and never reverses external effects, approvals or history. The host's automatic rollback after
//! a failed startup uses the same rule ([`PackageService::startup_failed`]). Neither restores a
//! version whose prerequisites are not met now.
//!
//! **Disable / uninstall.** Disable commits first (the package leaves the active set, so new
//! invocations are refused); the host then cancels and drains its invocations and retires the
//! process. Uninstall additionally clears the pointers and trust; executable files are removed
//! only after retirement ([`PackageService::collect_files`]), and package state, history and
//! provenance are retained.
//!
//! # Availability (ADDON-02)
//!
//! Effective availability is computed from current records, separately from configured
//! enablement: `NotInstalled`, `Disabled`, `Incompatible` (no prebuilt helper for this target,
//! protocol or host version), `MissingDependency` (a required package or runtime), `Failed` (the
//! helper failed for this version) or `Ready`. A missing or failed package affects only itself
//! and the packages that require it.
//!
//! # Trust model (EXT-04)
//!
//! A package helper is a separate native process the host launches, monitors and stops. That
//! gives failure containment and process-tree ownership, **not an OS sandbox**: the helper runs
//! with the desktop user's rights and can use any file or network access the OS allows it.
//! Signatures, publisher trust and approval establish who published which exact bytes and that a
//! person agreed to run them; they cannot prove the code is benign or detect undisclosed
//! authorship. The host's checks govern only brokered operations (state, events, and later the
//! SDK's host calls); requested access is an admission input and display, not containment.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::jcs;
use crate::operations::{OperationInput, addons as ops};
use crate::ports::packages::{
    FieldError, HostPlatform, InspectError, Inspection, MAX_BUNDLE_BYTES, PackageActivity,
    PackageFiles, PackageInspector, QuiescenceGuard, RuntimeProbe, SchemaValidator,
};
use crate::ports::storage::{
    ApplicationStore, NewEvent, PackageRegistryRead, ReadSnapshot, StorageError, StoredPackage,
    StoredPackageVersion, StoredProposal, StoredStateGeneration, VersionPhase, WriteOptions,
    WriteTransaction, run_write,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared, QueryAnswer,
    QueryHandler,
};
use crate::protocol::addons::AddonCommand;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::PackageWaitReason;
use crate::protocol::records::{DiagnosticErrorClass, DiagnosticOutcome, Severity};
use crate::protocol::results::{DomainCommandReply, DomainDecision, OperationResult};
use crate::protocol::{
    ActorId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, RevisionDimension,
};
use crate::services::artifacts::ArtifactService;
use crate::services::artifacts::lifecycle::ExactReadRefusal;
use crate::services::diagnostics::{DiagnosticService, LogEntry};
use crate::services::sessions::{Failure, Refusal, decided, query_fault, rev};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub mod index;
pub mod policy;
pub mod view;

/// The operations [`PackageService`] is registered for.
pub const OPERATIONS: [&str; 10] = [
    "addons.catalog",
    "addons.install",
    "addons.configure",
    "addons.enable",
    "addons.disable",
    "addons.update",
    "addons.propose",
    "addons.logs",
    "addons.rollback",
    "addons.uninstall",
];
/// History aggregate of package lifecycle events.
pub const AGGREGATE: &str = "package";
/// Diagnostics component of the package host (reserved to the host).
pub const COMPONENT: &str = "packages";
/// Diagnostic target kind of a package.
pub const TARGET_KIND: &str = "package";
/// Host-defined `addons.configure` schema: the package's update policy (publisher trust).
pub const UPDATE_POLICY_SCHEMA: &str = "devworks.packages/update-policy";
/// Host-defined `addons.configure` schema: per-project enablement.
pub const PROJECT_ENABLEMENT_SCHEMA: &str = "devworks.packages/project-enablement";
/// Default bound on waiting for a running package to quiesce inside a command.
pub const QUIESCE_WAIT: Duration = Duration::from_secs(2);
/// Most packages a registry view loads (availability needs the whole dependency graph).
pub const MAX_PACKAGES: usize = 4096;
/// Age after which an unreferenced install directory is collected outside boot.
pub const ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);

/// The canonical schema documents of the host-defined configuration schemas; a configure must
/// present exactly this schema reference (identity, revision and digest).
pub fn host_schema(id: &str) -> Option<Value> {
    match id {
        UPDATE_POLICY_SCHEMA => Some(json!({
            "$id": UPDATE_POLICY_SCHEMA,
            "type": "object",
            "additionalProperties": false,
            "required": ["trustPublisherUpdates"],
            "properties": {
                "trustPublisherUpdates": {"type": "boolean"},
                "releaseStream": {"enum": ["stable"]}
            }
        })),
        PROJECT_ENABLEMENT_SCHEMA => Some(json!({
            "$id": PROJECT_ENABLEMENT_SCHEMA,
            "type": "object",
            "additionalProperties": false,
            "required": ["disabledProjects"],
            "properties": {
                "disabledProjects": {"type": "array", "maxItems": 128, "items": {"type": "string"}}
            }
        })),
        _ => None,
    }
}
/// The exact `SchemaRef` (wire JSON) of a host-defined configuration schema.
pub fn host_schema_ref(id: &str) -> Option<Value> {
    let doc = host_schema(id)?;
    let canonical = jcs::canonicalize(&doc).ok()?;
    Some(json!({"schemaId": id, "revision": "1", "sha256": sha256_hex(canonical.as_bytes())}))
}

/// SHA-256 of a schema reference's canonical JSON.
pub fn schema_digest(schema: &Value) -> String {
    jcs::canonicalize(schema)
        .map(|c| sha256_hex(c.as_bytes()))
        .unwrap_or_else(|_| sha256_hex(schema.to_string().as_bytes()))
}

/// The package service and its host ports.
pub struct PackageService {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    artifacts: Arc<ArtifactService>,
    inspector: Arc<dyn PackageInspector>,
    files: Arc<dyn PackageFiles>,
    runtimes: Arc<dyn RuntimeProbe>,
    platform: HostPlatform,
    diagnostics: Arc<DiagnosticService>,
    activity: RwLock<Arc<dyn PackageActivity>>,
    validator: Arc<dyn SchemaValidator>,
    /// Consumers that receive every `PackageAvailabilityChanged` delivery.
    consumers: RwLock<Vec<String>>,
    /// Install directories a command published and has not settled yet: collection never
    /// removes them (a retired version being installed again is republished, then recorded).
    publishing: Arc<Mutex<BTreeMap<(String, String), usize>>>,
    /// Serializes publication with collection: a publish (including the reuse of an existing
    /// install directory) and collection's eligibility decision plus its physical removal never
    /// interleave.
    files_gate: Arc<Mutex<()>>,
    /// Bound on waiting for a running package to quiesce inside an update or rollback command.
    pub quiesce_wait: Duration,
}

/// A configuration refused by its schema: the diagnostic that carries the field errors, written
/// once the refusal is known (the gateway undoes a refusal's own writes).
#[derive(Default)]
struct ConfigRefusal(Mutex<Option<(Id, String, Vec<FieldError>)>>);

/// The quiescence guard a command holds from preparation until it settles.
struct GuardCell(Mutex<Option<Box<dyn QuiescenceGuard>>>);
impl GuardCell {
    fn new(g: Option<Box<dyn QuiescenceGuard>>) -> Self {
        Self(Mutex::new(g))
    }
    fn held(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }
    fn release(&self) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

/// What preparation established for a bundle command.
struct BundlePrep {
    outcome: Result<Inspection, (String, String)>,
    /// The package the command names (diagnostics of a refused bundle go there).
    target: Option<String>,
    /// The bundle artifact's creation provenance.
    provenance: Value,
    bundle: ArtifactRef,
    guard: GuardCell,
    /// Keeps the published install directory from collection until the command settles.
    _held: Option<PublishHold>,
}
/// An install directory published by a command that has not settled.
struct PublishHold {
    set: Arc<Mutex<BTreeMap<(String, String), usize>>>,
    key: (String, String),
}
impl PublishHold {
    fn new(set: &Arc<Mutex<BTreeMap<(String, String), usize>>>, key: (String, String)) -> Self {
        *set.lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(key.clone())
            .or_default() += 1;
        Self {
            set: set.clone(),
            key,
        }
    }
}
impl Drop for PublishHold {
    fn drop(&mut self) {
        let mut m = self.set.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = m.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.key);
            }
        }
    }
}
/// What preparation established for any lifecycle command.
enum Prep {
    Bundle(Box<BundlePrep>),
    Lifecycle(GuardCell),
    Configure(GuardCell, ConfigRefusal),
}

fn invalid(field: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, field, None)
}
fn not_found(kind: &'static str, id: &str) -> Refusal {
    Refusal::new(DomainErrorCode::NotFound, kind, Id::new(id).ok())
}
fn conflict(kind: &'static str, id: &str) -> Refusal {
    Refusal::new(DomainErrorCode::Conflict, kind, Id::new(id).ok())
}
fn registry_conflict(expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Registry,
        "package_registry",
        Id::new("registry").expect("valid id"),
        rev(expected),
        rev(current),
    )
}
fn package_conflict(
    package: &str,
    dimension: RevisionDimension,
    expected: i64,
    current: i64,
) -> StorageError {
    StorageError::conflict(
        dimension,
        "package",
        Id::new(package).unwrap_or_else(|_| Id::new("package").expect("valid id")),
        rev(expected),
        rev(current),
    )
}

/// The context's principal kind as recorded.
fn kind_name(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::Human => "Human",
        PrincipalKind::Agent => "Agent",
        PrincipalKind::Package => "Package",
        PrincipalKind::McpClient => "McpClient",
        PrincipalKind::Schedule => "Schedule",
        PrincipalKind::Host => "Host",
    }
}

/// History event of a package transition.
pub(crate) fn event(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    p: &StoredPackage,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: actor.clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: AGGREGATE.into(),
        aggregate_id: p.package_id.clone(),
        aggregate_revision: p.revision.max(1),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

impl PackageService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        artifacts: Arc<ArtifactService>,
        inspector: Arc<dyn PackageInspector>,
        files: Arc<dyn PackageFiles>,
        runtimes: Arc<dyn RuntimeProbe>,
        platform: HostPlatform,
        diagnostics: Arc<DiagnosticService>,
        validator: Arc<dyn SchemaValidator>,
    ) -> Self {
        Self {
            store,
            ids,
            artifacts,
            inspector,
            files,
            runtimes,
            platform,
            diagnostics,
            activity: RwLock::new(Arc::new(crate::ports::packages::Idle)),
            validator,
            consumers: RwLock::new(Vec::new()),
            publishing: Arc::new(Mutex::new(BTreeMap::new())),
            files_gate: Arc::new(Mutex::new(())),
            quiesce_wait: QUIESCE_WAIT,
        }
    }
    /// Registers the consumers that receive availability announcements (composition root).
    pub fn set_availability_consumers(&self, consumers: Vec<String>) {
        *self.consumers.write().unwrap_or_else(|p| p.into_inner()) = consumers;
    }
    pub(crate) fn consumers(&self) -> Vec<String> {
        self.consumers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Inspects bundle bytes with the service's inspector (nothing is published or trusted).
    pub fn inspect(&self, bundle: &[u8]) -> Result<Inspection, InspectError> {
        self.inspector.inspect(bundle).map(|b| b.inspection)
    }
    /// The schema validator of configuration values.
    pub fn validator(&self) -> &Arc<dyn SchemaValidator> {
        &self.validator
    }
    /// Installs the package host's activity view (once, at boot).
    pub fn set_activity(&self, activity: Arc<dyn PackageActivity>) {
        *self.activity.write().unwrap_or_else(|p| p.into_inner()) = activity;
    }
    fn activity(&self) -> Arc<dyn PackageActivity> {
        self.activity
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn platform(&self) -> &HostPlatform {
        &self.platform
    }
    pub fn files(&self) -> &Arc<dyn PackageFiles> {
        &self.files
    }
    pub fn runtimes(&self) -> &Arc<dyn RuntimeProbe> {
        &self.runtimes
    }
    pub fn diagnostics(&self) -> &Arc<DiagnosticService> {
        &self.diagnostics
    }
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Records a package diagnostic in the caller's unit of work.
    pub(crate) fn log_in(
        &self,
        tx: &mut dyn WriteTransaction,
        package_id: &str,
        severity: Severity,
        code: &str,
        message: &str,
        outcome: Option<DiagnosticOutcome>,
    ) -> Result<(), StorageError> {
        let mut e = LogEntry::new(COMPONENT, code, severity, message);
        e.target_kind = Some(TARGET_KIND.into());
        e.target_id = Id::new(package_id).ok();
        let failed = matches!(outcome, Some(DiagnosticOutcome::Failed));
        e.outcome = outcome;
        if failed {
            e.error_class = Some(DiagnosticErrorClass::Dependency);
        }
        match self.diagnostics.log_in(tx, &e) {
            Ok(_) => Ok(()),
            Err(crate::services::diagnostics::LogRefusal::Storage(s)) => Err(s),
            Err(_) => Ok(()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Preparation (outside the unit of work)

    fn prepare_bundle(
        &self,
        ctx: &InvocationContext,
        bundle: &ArtifactRef,
        quiesce: Option<&str>,
    ) -> BundlePrep {
        let read = self
            .artifacts
            .read_exact(&*self.store, ctx, bundle, MAX_BUNDLE_BYTES);
        let guard = GuardCell::new(None);
        let (bytes, provenance) = match read {
            Ok(b) => b,
            Err(e) => {
                let (code, detail) = match e {
                    ExactReadRefusal::NotFound => (
                        "BundleUnavailable",
                        "The bundle artifact is not available".into(),
                    ),
                    ExactReadRefusal::TooLarge { bytes, max } => (
                        "BundleTooLarge",
                        format!("The bundle is {bytes} bytes; the limit is {max}"),
                    ),
                    ExactReadRefusal::Unreadable => (
                        "BundleUnavailable",
                        "The bundle artifact cannot be read".into(),
                    ),
                    ExactReadRefusal::Storage(e) => ("BundleUnavailable", format!("{e:?}")),
                };
                return BundlePrep {
                    target: quiesce.map(str::to_owned),
                    outcome: Err((code.into(), detail)),
                    provenance: Value::Null,
                    bundle: bundle.clone(),
                    guard,
                    _held: None,
                };
            }
        };
        let inspected = match self.inspector.inspect(&bytes) {
            Ok(i) => i,
            Err(e) => {
                return BundlePrep {
                    target: quiesce.map(str::to_owned),
                    outcome: Err((e.code().into(), e.detail().to_owned())),
                    provenance,
                    bundle: bundle.clone(),
                    guard,
                    _held: None,
                };
            }
        };
        let i = inspected.inspection;
        // The hold and the publication happen while no collection pass decides or removes.
        let gate = self.files_gate.lock().unwrap_or_else(|p| p.into_inner());
        let held = PublishHold::new(
            &self.publishing,
            (i.package_id.clone(), i.manifest_digest.clone()),
        );
        let published = self.files.publish(
            &i.package_id,
            &i.manifest_digest,
            &inspected.files,
            &i.files,
        );
        drop(gate);
        if let Err(e) = published {
            return BundlePrep {
                target: quiesce.map(str::to_owned),
                outcome: Err(("InstallFailed".into(), e.to_string())),
                provenance,
                bundle: bundle.clone(),
                guard,
                _held: None,
            };
        }
        if let Some(p) = quiesce.filter(|p| *p == i.package_id) {
            *guard.0.lock().unwrap_or_else(|e| e.into_inner()) =
                self.activity().quiesce(p, self.quiesce_wait);
        }
        BundlePrep {
            target: quiesce.map(str::to_owned),
            outcome: Ok(i),
            provenance,
            bundle: bundle.clone(),
            guard,
            _held: Some(held),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Shared rules

    fn package_or_new(
        read: &dyn PackageRegistryRead,
        package_id: &str,
        now: i64,
    ) -> Result<(StoredPackage, bool), StorageError> {
        Ok(match read.package(package_id)? {
            Some(p) => (p, false),
            None => (
                StoredPackage {
                    package_id: package_id.to_owned(),
                    revision: 0,
                    enabled: false,
                    installed: false,
                    active_digest: None,
                    predecessor_digest: None,
                    pending_digest: None,
                    state_generation: 0,
                    config: None,
                    config_revision: 0,
                    trust: None,
                    failure: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                },
                true,
            ),
        })
    }

    /// Writes `p` (insert or compare-and-set) and returns it at its new revision.
    pub(crate) fn save(
        tx: &mut dyn WriteTransaction,
        p: &mut StoredPackage,
        fresh: bool,
    ) -> Result<(), StorageError> {
        p.updated_at_ms = tx.now_ms();
        *p = if fresh {
            tx.packages().insert_package(p)?
        } else {
            let expected = p.revision;
            tx.packages().update_package(p, expected)?
        };
        Ok(())
    }

    fn check_registry(tx: &mut dyn WriteTransaction, expected: i64) -> Result<i64, StorageError> {
        let current = tx.packages().registry_revision()?;
        if current != expected {
            return Err(registry_conflict(expected, current));
        }
        Ok(current)
    }

    /// The waits `candidate` must clear before it may become active, and the approval it
    /// carries if none remain for approval.
    fn waits(
        ctx: &InvocationContext,
        candidate: &Inspection,
        version: &StoredPackageVersion,
        agent_authored: bool,
        current: Option<(&StoredPackage, &Inspection)>,
    ) -> (Vec<PackageWaitReason>, Option<Value>) {
        if ctx.principal_kind() == PrincipalKind::Human {
            return (Vec::new(), Some(json!({"kind": "Person"})));
        }
        if let Some(a) = &version.approval {
            // These exact contents were approved before (or activated under trust).
            return (Vec::new(), Some(a.clone()));
        }
        if agent_authored {
            return (vec![PackageWaitReason::ExactVersionApproval], None);
        }
        if let Some((p, active)) = current
            && let Some(trust) = p.trust.as_ref().and_then(policy::Trust::from_record)
        {
            return match policy::auto_update(&trust, active, candidate) {
                Ok(()) => (
                    Vec::new(),
                    Some(
                        json!({"kind": "TrustedPublisher", "publisherId": trust.publisher_id, "signerKey": trust.signer_key}),
                    ),
                ),
                Err(why) => (vec![why.wait()], None),
            };
        }
        if candidate.signer_key.is_some() {
            (vec![PackageWaitReason::TrustApproval], None)
        } else {
            (vec![PackageWaitReason::ExactVersionApproval], None)
        }
    }

    // -----------------------------------------------------------------------------------------
    // Use cases

    #[allow(clippy::too_many_arguments)]
    fn install_or_update(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        prep: Option<&BundlePrep>,
        target: Option<&Id>,
        expected_registry: i64,
        update: bool,
    ) -> Result<DomainDecision, Failure> {
        let Some(prep) = prep else {
            return Err(
                Refusal::new(DomainErrorCode::StorageUnavailable, "package_bundle", None)
                    .retry(crate::protocol::RetryAdvice::SameCommand)
                    .into(),
            );
        };
        let i = match &prep.outcome {
            Ok(i) => i.clone(),
            Err((code, _)) => {
                let code = if code == "BundleUnavailable" {
                    DomainErrorCode::NotFound
                } else {
                    DomainErrorCode::InvalidArgument
                };
                return Err(Refusal::new(
                    code,
                    "package_bundle",
                    Some(prep.bundle.version_id.clone()),
                )
                .into());
            }
        };
        if let Some(t) = target
            && t.as_str() != i.package_id
        {
            return Err(invalid("package_id").into());
        }
        Self::check_registry(tx, expected_registry)?;
        let now = tx.now_ms();
        let (mut p, fresh) = Self::package_or_new(&*tx.packages(), &i.package_id, now)?;
        if update && (!p.installed || p.active_digest.is_none()) {
            return Err(not_found("package", &i.package_id).into());
        }
        if !update && p.installed {
            return Err(conflict("package", &i.package_id).into());
        }
        let current_version = match &p.active_digest {
            Some(d) => tx.packages().package_version(&p.package_id, d)?,
            None => None,
        };
        let current = current_version.as_ref().and_then(policy::inspection);
        if update && p.active_digest.as_deref() == Some(i.manifest_digest.as_str()) {
            return Ok(DomainDecision::NoChange {
                result: self.result(tx, &p, update_op(update))?,
            });
        }
        // The package record exists before anything refers to it.
        if fresh {
            Self::save(tx, &mut p, true)?;
        }
        // The immutable version record (identity = manifest digest).
        let proposal = tx
            .packages()
            .proposal_for(&i.package_id, &i.manifest_digest)?;
        let agent_authored =
            proposal.is_some() || prep.provenance["principalKind"].as_str() == Some("Agent");
        let mut version = match tx
            .packages()
            .package_version(&i.package_id, &i.manifest_digest)?
        {
            Some(v) if v.phase == VersionPhase::Rejected => {
                return Err(conflict("package_version", &i.package_id).into());
            }
            Some(v) => v,
            None => {
                let v = StoredPackageVersion {
                    package_id: i.package_id.clone(),
                    manifest_digest: i.manifest_digest.clone(),
                    bundle_digest: i.bundle_digest.clone(),
                    version: i.version.clone(),
                    publisher_id: i.publisher_id.clone(),
                    signer_key: i.signer_key.clone(),
                    inspection: serde_json::to_value(&i)
                        .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                    provenance: json!({
                        "installedBy": ctx.principal().0.as_str(),
                        "principalKind": kind_name(ctx.principal_kind()),
                        "sessionId": ctx.session_id().map(Id::as_str),
                        "agentAuthored": agent_authored,
                        "proposalId": proposal.as_ref().map(|p| p.id.as_str()),
                        "source": prep.bundle,
                        "artifactProvenance": prep.provenance,
                    }),
                    phase: VersionPhase::Staged,
                    waits: Vec::new(),
                    approval: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                };
                tx.packages().insert_version(&v)?;
                v
            }
        };
        let (mut waits, approval) = Self::waits(
            ctx,
            &i,
            &version,
            agent_authored,
            current.as_ref().map(|c| (&p, c)),
        );
        let approval = approval.map(|mut a| {
            if let Value::Object(m) = &mut a {
                m.entry("by").or_insert(json!(ctx.principal().0.as_str()));
                m.entry("atMs").or_insert(json!(now));
                if let Some(pr) = &proposal {
                    m.entry("proposalId").or_insert(json!(pr.id.as_str()));
                }
            }
            a
        });
        // An update never replaces a working version with one that cannot run here, nor breaks
        // another package (see `policy::activation_regresses`).
        if current.is_some() && self.regresses(tx, &p, &version)?.is_some() {
            waits.push(PackageWaitReason::Compatibility);
        }
        let running = p.enabled && p.active_digest.is_some();
        if waits.is_empty() && running && !prep.guard.held() {
            waits.push(PackageWaitReason::Quiescence);
        }
        // A person's approval of these exact contents settles the proposal for them.
        if let (Some(a), Some(pr)) = (&approval, &proposal)
            && pr.state == "Pending"
            && ctx.principal_kind() == PrincipalKind::Human
        {
            let mut decided_pr = pr.clone();
            decided_pr.state = "Approved".into();
            decided_pr.decided_by = Some(ctx.principal().clone());
            decided_pr.decided_at_ms = Some(now);
            decided_pr.decision =
                Some(json!({"decision": "Approve", "via": update_op(update), "approval": a}));
            let expected = pr.revision;
            tx.packages().update_proposal(&decided_pr, expected)?;
        }
        let actor = ctx.principal().clone();
        if waits.is_empty() {
            version.approval = approval.clone();
            if !update {
                p.installed = true;
                p.enabled = true;
                p.failure = None;
            }
            self.activate(
                tx,
                &actor,
                &mut p,
                &version,
                if update { "Update" } else { "Install" },
            )?;
        } else {
            let names: Vec<String> = waits
                .iter()
                .map(|w| policy::wait_name(w).to_owned())
                .collect();
            // An approval recorded now (a person's) stays with the version even while it waits
            // for quiescence or compatibility.
            let keep = approval
                .as_ref()
                .filter(|_| ctx.principal_kind() == PrincipalKind::Human);
            tx.packages().update_version(
                &i.package_id,
                &i.manifest_digest,
                VersionPhase::Staged,
                &names,
                keep.or(version.approval.as_ref()),
            )?;
            if !update && !p.installed {
                // A first install that waits is listed with its pending version.
                p.enabled = false;
            }
            Self::retire_obsolete_pending(tx, &actor, &mut p, &i.manifest_digest)?;
            p.pending_digest = Some(i.manifest_digest.clone());
            Self::save(tx, &mut p, false)?;
            event(
                tx,
                &actor,
                &p,
                "PackageStaged",
                json!({"digest": i.manifest_digest, "version": i.version, "waits": names}),
            )?;
            self.log_in(
                tx,
                &p.package_id,
                Severity::Info,
                "PackageStaged",
                &format!(
                    "Version {} is staged and waits for {}",
                    i.version,
                    names.join(", ")
                ),
                Some(DiagnosticOutcome::Running),
            )?;
        }
        Ok(DomainDecision::Applied {
            result: self.result(tx, &p, update_op(update))?,
        })
    }

    /// Whether activating `next` in place of `p`'s active version would regress it or another
    /// package against the current registry and runtimes ([`policy::activation_regresses`]).
    fn regresses(
        &self,
        tx: &mut dyn WriteTransaction,
        p: &StoredPackage,
        next: &StoredPackageVersion,
    ) -> Result<Option<String>, StorageError> {
        let reg = self.registry(&*tx.packages())?;
        let view = policy::Registry {
            packages: &reg,
            runtimes: &*self.runtimes,
            host: &self.platform,
        };
        Ok(policy::activation_regresses(&view, &p.package_id, next))
    }

    /// Publishes `version` as the active version of `p` (see the module documentation,
    /// "Publication"): new state generation, pointer, approval, registry head, history.
    pub(crate) fn activate(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        p: &mut StoredPackage,
        version: &StoredPackageVersion,
        cause: &str,
    ) -> Result<(), StorageError> {
        let i = policy::inspection(version)
            .ok_or_else(|| StorageError::InvalidRecord("package version inspection".into()))?;
        let schema = schema_digest(&i.state_schema);
        let carried = if p.state_generation > 0 {
            tx.packages()
                .generation_state(&p.package_id, p.state_generation)?
        } else {
            None
        };
        let generation = p.state_generation + 1;
        let now = tx.now_ms();
        // Wire revisions are positive: a generation without state starts at revision 1.
        let (state, revision) = match &carried {
            Some((r, s)) => (Some(s.clone()), (*r).max(1)),
            None => (None, 1),
        };
        tx.packages().publish_generation(
            &StoredStateGeneration {
                package_id: p.package_id.clone(),
                generation,
                manifest_digest: version.manifest_digest.clone(),
                schema_digest: schema,
                seeded_from: (p.state_generation > 0).then_some(p.state_generation),
                seed_revision: revision,
                created_at_ms: now,
            },
            state.as_ref(),
            revision,
        )?;
        self.point(tx, actor, p, version, generation, cause)
    }

    /// Moves the active pointer to `version` at `generation` (already published).
    fn point(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        p: &mut StoredPackage,
        version: &StoredPackageVersion,
        generation: i64,
        cause: &str,
    ) -> Result<(), StorageError> {
        if p.active_digest.as_deref() != Some(version.manifest_digest.as_str()) {
            p.predecessor_digest = p.active_digest.take();
        }
        p.active_digest = Some(version.manifest_digest.clone());
        if p.pending_digest.as_deref() == Some(version.manifest_digest.as_str()) {
            p.pending_digest = None;
        }
        Self::retire_obsolete_pending(tx, actor, p, &version.manifest_digest)?;
        p.state_generation = generation;
        // Activating a version clears its own failure record (an explicit retry); another
        // version's record stays, so automatic rollback never returns to a failed version.
        if failure_of(p, &version.manifest_digest).is_some() {
            p.failure = None;
        }
        tx.packages().update_version(
            &p.package_id,
            &version.manifest_digest,
            VersionPhase::Installed,
            &[],
            version
                .approval
                .as_ref()
                .or(Some(&json!({"kind": "Activated", "by": actor.0.as_str()}))),
        )?;
        Self::save(tx, p, false)?;
        let head = tx.packages().registry_revision()?;
        let registry = tx.packages().advance_registry(head)?;
        event(
            tx,
            actor,
            p,
            "PackageActivated",
            json!({
                "digest": version.manifest_digest,
                "version": version.version,
                "registryRevision": registry,
                "stateGeneration": generation,
                "cause": cause,
                "approval": version.approval,
            }),
        )?;
        self.log_in(
            tx,
            &p.package_id,
            Severity::Info,
            "PackageActivated",
            &format!(
                "Version {} is active (state generation {generation}, {cause})",
                version.version
            ),
            Some(DiagnosticOutcome::Succeeded),
        )
    }

    /// Another version became active or pending explicitly (`by`): a pending version that waited
    /// only for the host (quiescence or compatibility, its approval already recorded) is an
    /// obsolete activation request, retired (its files go, its identity and approval stay) so a
    /// later retry never replaces that explicit choice with it. A pending version still waiting
    /// on a person's decision stays pending.
    fn retire_obsolete_pending(
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        p: &mut StoredPackage,
        by: &str,
    ) -> Result<(), StorageError> {
        if let Some(obsolete) = p.pending_digest.clone().filter(|d| d != by)
            && let Some(pv) = tx.packages().package_version(&p.package_id, &obsolete)?
            && pv.phase == VersionPhase::Staged
            && retryable(&pv.waits)
        {
            tx.packages().update_version(
                &p.package_id,
                &obsolete,
                VersionPhase::Removed,
                &[],
                pv.approval.as_ref(),
            )?;
            p.pending_digest = None;
            event(
                tx,
                actor,
                p,
                "PackagePendingRetired",
                json!({"digest": obsolete, "version": pv.version, "supersededBy": by}),
            )?;
        }
        Ok(())
    }

    /// Advances the registry head after a change to the active set that is not an activation.
    fn republish(
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        p: &mut StoredPackage,
        kind: &str,
        payload: Value,
    ) -> Result<i64, StorageError> {
        Self::save(tx, p, false)?;
        let head = tx.packages().registry_revision()?;
        let registry = tx.packages().advance_registry(head)?;
        let mut payload = payload;
        if let Value::Object(m) = &mut payload {
            m.insert("registryRevision".into(), json!(registry));
        }
        event(tx, actor, p, kind, payload)?;
        Ok(registry)
    }

    fn enable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        package_id: &Id,
        expected: i64,
    ) -> Result<DomainDecision, Failure> {
        Self::check_registry(tx, expected)?;
        let mut p = tx
            .packages()
            .package(package_id.as_str())?
            .filter(|p| p.installed && p.active_digest.is_some())
            .ok_or_else(|| not_found("package", package_id.as_str()))?;
        if p.enabled
            && p.active_digest
                .as_deref()
                .is_none_or(|d| failure_of(&p, d).is_none())
        {
            return Ok(DomainDecision::NoChange {
                result: self.result(tx, &p, "addons.enable")?,
            });
        }
        // Enabling again is also the explicit retry of a failed helper.
        p.enabled = true;
        p.failure = None;
        Self::republish(tx, ctx.principal(), &mut p, "PackageEnabled", json!({}))?;
        self.log_in(
            tx,
            &p.package_id,
            Severity::Info,
            "PackageEnabled",
            "The package was enabled",
            None,
        )?;
        Ok(DomainDecision::Applied {
            result: self.result(tx, &p, "addons.enable")?,
        })
    }

    fn disable(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        package_id: &Id,
        expected: i64,
    ) -> Result<DomainDecision, Failure> {
        Self::check_registry(tx, expected)?;
        let mut p = tx
            .packages()
            .package(package_id.as_str())?
            .filter(|p| p.installed)
            .ok_or_else(|| not_found("package", package_id.as_str()))?;
        if !p.enabled {
            return Ok(DomainDecision::NoChange {
                result: self.result(tx, &p, "addons.disable")?,
            });
        }
        p.enabled = false;
        Self::republish(tx, ctx.principal(), &mut p, "PackageDisabled", json!({}))?;
        self.log_in(
            tx,
            &p.package_id,
            Severity::Info,
            "PackageDisabled",
            "The package was disabled; its invocations are cancelled and its helper retired",
            None,
        )?;
        Ok(DomainDecision::Applied {
            result: self.result(tx, &p, "addons.disable")?,
        })
    }

    fn uninstall(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        package_id: &Id,
        expected: i64,
    ) -> Result<DomainDecision, Failure> {
        Self::check_registry(tx, expected)?;
        let mut p = tx
            .packages()
            .package(package_id.as_str())?
            .filter(|p| p.installed)
            .ok_or_else(|| not_found("package", package_id.as_str()))?;
        let removed = p.active_digest.clone();
        p.installed = false;
        p.enabled = false;
        p.active_digest = None;
        p.predecessor_digest = None;
        p.trust = None;
        p.failure = None;
        // A staged candidate of an uninstalled package never activates by itself: it is retired
        // (its files go, its identity and provenance stay), not rejected, so a later explicit
        // install of the same contents goes through the ordinary approval path again. A pending
        // proposal for it expires.
        if let Some(d) = p.pending_digest.take() {
            tx.packages()
                .update_version(&p.package_id, &d, VersionPhase::Removed, &[], None)?;
            if let Some(pr) = tx.packages().proposal_for(&p.package_id, &d)?
                && pr.state == "Pending"
            {
                let mut e = pr.clone();
                e.state = "Expired".into();
                e.decided_at_ms = Some(tx.now_ms());
                e.decision = Some(json!({"expiredBy": "addons.uninstall"}));
                tx.packages().update_proposal(&e, pr.revision)?;
            }
        }
        let payload = json!({"digest": removed, "stateGeneration": p.state_generation, "stateRetained": true});
        Self::republish(tx, ctx.principal(), &mut p, "PackageUninstalled", payload)?;
        self.log_in(tx, &p.package_id, Severity::Info, "PackageUninstalled", "The package was uninstalled; its state and history are retained and its files are removed after its helper is retired", None)?;
        Ok(DomainDecision::Applied {
            result: self.result(tx, &p, "addons.uninstall")?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn rollback(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        package_id: &Id,
        target: &str,
        expected_registry: i64,
        expected_generation: i64,
        guard: Option<&GuardCell>,
    ) -> Result<DomainDecision, Failure> {
        Self::check_registry(tx, expected_registry)?;
        let mut p = tx
            .packages()
            .package(package_id.as_str())?
            .filter(|p| p.installed && p.active_digest.is_some())
            .ok_or_else(|| not_found("package", package_id.as_str()))?;
        if p.state_generation != expected_generation {
            return Err(package_conflict(
                &p.package_id,
                RevisionDimension::Package,
                expected_generation,
                p.state_generation,
            )
            .into());
        }
        if p.active_digest.as_deref() == Some(target) {
            return Ok(DomainDecision::NoChange {
                result: self.result(tx, &p, "addons.rollback")?,
            });
        }
        let version = tx
            .packages()
            .package_version(&p.package_id, target)?
            .filter(|v| v.phase == VersionPhase::Installed && v.approval.is_some())
            .ok_or_else(|| not_found("package_version", &p.package_id))?;
        let Some(ti) = policy::inspection(&version) else {
            return Err(conflict("package_version", &p.package_id).into());
        };
        if policy::static_incompatibility(&ti, &self.platform).is_some() {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedTarget,
                "package_version",
                Id::new(&p.package_id).ok(),
            )
            .into());
        }
        // Like an update, a rollback never replaces a working version with one whose
        // prerequisites are not met now.
        if self.regresses(tx, &p, &version)?.is_some() {
            return Err(Refusal::new(
                DomainErrorCode::UnsupportedTarget,
                "package_compatibility",
                Id::new(&p.package_id).ok(),
            )
            .into());
        }
        if p.enabled && !guard.is_some_and(GuardCell::held) {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "package_quiescence",
                Id::new(&p.package_id).ok(),
            )
            .retry(crate::protocol::RetryAdvice::SameCommand)
            .into());
        }
        let actor = ctx.principal().clone();
        match self.restore(tx, &actor, &mut p, &version, "Rollback")? {
            true => Ok(DomainDecision::Applied {
                result: self.result(tx, &p, "addons.rollback")?,
            }),
            false => Err(conflict("package_state", &p.package_id).into()),
        }
    }

    /// Activates the retained `version` under the guarded state rule (module documentation,
    /// "Rollback"). `false` (nothing written) when its state is incompatible with later work.
    pub(crate) fn restore(
        &self,
        tx: &mut dyn WriteTransaction,
        actor: &ActorId,
        p: &mut StoredPackage,
        version: &StoredPackageVersion,
        cause: &str,
    ) -> Result<bool, StorageError> {
        let Some(ti) = policy::inspection(version) else {
            return Ok(false);
        };
        let target_schema = schema_digest(&ti.state_schema);
        let current = tx
            .packages()
            .state_generation(&p.package_id, p.state_generation)?;
        let Some(current) = current else {
            // No state was ever published: nothing can be lost.
            self.activate(tx, actor, p, version, cause)?;
            return Ok(true);
        };
        if current.schema_digest == target_schema {
            self.activate(tx, actor, p, version, cause)?;
            return Ok(true);
        }
        let now_revision = tx
            .packages()
            .generation_state(&p.package_id, p.state_generation)?
            .map_or(0, |(r, _)| r);
        let Some(seed) = current
            .seeded_from
            .filter(|_| now_revision == current.seed_revision)
        else {
            return Ok(false);
        };
        let Some(pred) = tx.packages().state_generation(&p.package_id, seed)? else {
            return Ok(false);
        };
        if pred.manifest_digest != version.manifest_digest && pred.schema_digest != target_schema {
            return Ok(false);
        }
        let restored = tx.packages().generation_state(&p.package_id, seed)?;
        let generation = p.state_generation + 1;
        let now = tx.now_ms();
        let (state, revision) = match &restored {
            Some((r, s)) => (Some(s.clone()), (*r).max(1)),
            None => (None, 1),
        };
        tx.packages().publish_generation(
            &StoredStateGeneration {
                package_id: p.package_id.clone(),
                generation,
                manifest_digest: version.manifest_digest.clone(),
                schema_digest: target_schema,
                seeded_from: Some(seed),
                seed_revision: revision,
                created_at_ms: now,
            },
            state.as_ref(),
            revision,
        )?;
        self.point(tx, actor, p, version, generation, cause)?;
        Ok(true)
    }

    fn configure(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ConfigureInput,
        refusal: Option<&ConfigRefusal>,
    ) -> Result<DomainDecision, Failure> {
        let mut p = tx
            .packages()
            .package(i.package_id.as_str())?
            .filter(|p| p.installed)
            .ok_or_else(|| not_found("package", i.package_id.as_str()))?;
        if p.revision != i.expected_revision.get() {
            return Err(package_conflict(
                &p.package_id,
                RevisionDimension::Configuration,
                i.expected_revision.get(),
                p.revision,
            )
            .into());
        }
        let schema = serde_json::to_value(&i.config.schema)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let value = serde_json::to_value(&i.config.value)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let schema_id = schema["schemaId"].as_str().unwrap_or_default().to_owned();
        let actor = ctx.principal().clone();
        if host_schema(&schema_id).is_some() {
            if Some(&schema) != host_schema_ref(&schema_id).as_ref() {
                return Err(Refusal::new(
                    DomainErrorCode::SchemaMismatch,
                    "package_configuration",
                    None,
                )
                .into());
            }
            if let Some(doc) = host_schema(&schema_id)
                && let Err(errors) = self.validator.validate(&doc, &value)
            {
                return Ok(Self::config_refused(tx, &p.package_id, errors, refusal));
            }
            if schema_id == UPDATE_POLICY_SCHEMA {
                return self.update_policy(tx, ctx, &mut p, &value);
            }
            let Some(list) = value
                .as_object()
                .filter(|o| o.len() == 1)
                .and_then(|o| o.get("disabledProjects"))
                .and_then(Value::as_array)
                .filter(|a| a.len() <= 128)
            else {
                return Err(invalid("config").into());
            };
            let mut projects = BTreeSet::new();
            for v in list {
                let Some(id) = v.as_str().and_then(|s| Id::new(s).ok()) else {
                    return Err(invalid("config").into());
                };
                projects.insert(id.as_str().to_owned());
            }
            let mut config = p.config.clone().unwrap_or_else(|| json!({}));
            config["disabledProjects"] = json!(projects);
            p.config = Some(config);
        } else {
            let active = match &p.active_digest {
                Some(d) => tx.packages().package_version(&p.package_id, d)?,
                None => None,
            };
            let Some(ai) = active.as_ref().and_then(policy::inspection) else {
                return Err(not_found("package_version", &p.package_id).into());
            };
            // Exactly the installed configuration schema (identity, revision and digest), and a
            // value that its shipped schema document accepts.
            if schema != ai.configuration_schema {
                return Err(Refusal::new(
                    DomainErrorCode::SchemaMismatch,
                    "package_configuration",
                    Id::new(&p.package_id).ok(),
                )
                .into());
            }
            let Some(doc) = ai.configuration_schema_document.as_ref() else {
                // Nothing can be validated against a schema the package does not ship.
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "package_configuration",
                    Id::new(&p.package_id).ok(),
                )
                .into());
            };
            if let Err(errors) = self.validator.validate(doc, &value) {
                return Ok(Self::config_refused(tx, &p.package_id, errors, refusal));
            }
            let mut config = p.config.clone().unwrap_or_else(|| json!({}));
            config["schema"] = schema;
            config["value"] = value;
            p.config = Some(config);
        }
        p.config_revision += 1;
        // The helper instance is keyed by configuration revision: it restarts at its next safe
        // boundary, so the active set changes.
        let payload = json!({"configurationRevision": p.config_revision, "schemaId": schema_id});
        Self::republish(tx, &actor, &mut p, "PackageConfigured", payload)?;
        Ok(DomainDecision::Applied {
            result: self.result(tx, &p, "addons.configure")?,
        })
    }

    fn update_policy(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        p: &mut StoredPackage,
        value: &Value,
    ) -> Result<DomainDecision, Failure> {
        // Publisher trust is an explicit act of the person, never of an agent or package.
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "publisher_trust",
                Id::new(&p.package_id).ok(),
            )
            .into());
        }
        let Some(o) = value.as_object() else {
            return Err(invalid("config").into());
        };
        let allowed = ["trustPublisherUpdates", "releaseStream"];
        let trust = o.get("trustPublisherUpdates").and_then(Value::as_bool);
        let stream = o.get("releaseStream").map_or(Some("stable"), Value::as_str);
        let (Some(trust), Some("stable")) = (trust, stream) else {
            return Err(invalid("config").into());
        };
        if o.keys().any(|k| !allowed.contains(&k.as_str())) {
            return Err(invalid("config").into());
        }
        let actor = ctx.principal().clone();
        if trust {
            let active = match &p.active_digest {
                Some(d) => tx.packages().package_version(&p.package_id, d)?,
                None => None,
            };
            let Some(v) = active else {
                return Err(not_found("package_version", &p.package_id).into());
            };
            let Some(key) = v.signer_key.clone() else {
                // Unsigned contents cannot establish a publisher to trust.
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "publisher_trust",
                    Id::new(&p.package_id).ok(),
                )
                .into());
            };
            let access = policy::inspection(&v).map(|i| i.access).unwrap_or_default();
            p.trust = Some(json!({
                "publisherId": v.publisher_id,
                "signerKey": key,
                "packageId": p.package_id,
                "stream": "stable",
                "approvedAccess": access,
                "trustedBy": actor.0.as_str(),
                "atMs": tx.now_ms(),
                "fromVersion": v.version,
            }));
        } else {
            p.trust = None;
        }
        p.config_revision += 1;
        Self::save(tx, p, false)?;
        event(
            tx,
            &actor,
            p,
            if trust {
                "PublisherTrusted"
            } else {
                "PublisherTrustRevoked"
            },
            json!({"trust": p.trust}),
        )?;
        self.log_in(tx, &p.package_id, Severity::Info, if trust { "PublisherTrusted" } else { "PublisherTrustRevoked" }, if trust { "Signed compatible updates from this publisher may now activate automatically within the approved access" } else { "Updates from this publisher need explicit approval again" }, None)?;
        Ok(DomainDecision::Applied {
            result: self.result(tx, p, "addons.configure")?,
        })
    }

    fn propose(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::ProposeInput,
        prep: Option<&BundlePrep>,
    ) -> Result<DomainDecision, Failure> {
        let Some(prep) = prep else {
            return Err(
                Refusal::new(DomainErrorCode::StorageUnavailable, "package_bundle", None).into(),
            );
        };
        let candidate = match &prep.outcome {
            Ok(c) => c.clone(),
            Err(_) => {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "package_bundle",
                    Some(prep.bundle.version_id.clone()),
                )
                .into());
            }
        };
        // Approval binds to the reviewed contents: the caller names the exact manifest digest.
        if String::from(i.manifest_digest.clone()) != candidate.manifest_digest {
            return Err(
                Refusal::new(DomainErrorCode::SourceChanged, "package_manifest", None).into(),
            );
        }
        if let Some(base) = &i.base_package_id
            && base.as_str() != candidate.package_id
        {
            return Err(invalid("base_package_id").into());
        }
        let now = tx.now_ms();
        let (mut p, fresh) = Self::package_or_new(&*tx.packages(), &candidate.package_id, now)?;
        if let Some(expected) = &i.expected_revision
            && p.revision != expected.get()
        {
            return Err(package_conflict(
                &p.package_id,
                RevisionDimension::Package,
                expected.get(),
                p.revision,
            )
            .into());
        }
        if p.active_digest.as_deref() == Some(candidate.manifest_digest.as_str()) {
            return Err(conflict("package_version", &p.package_id).into());
        }
        let base_version = match &p.active_digest {
            Some(d) => tx.packages().package_version(&p.package_id, d)?,
            None => None,
        };
        let base = base_version.as_ref().and_then(policy::inspection);
        if fresh {
            Self::save(tx, &mut p, true)?;
        }
        let id = tx.new_id(IdKind::Proposal);
        let existing = tx
            .packages()
            .package_version(&candidate.package_id, &candidate.manifest_digest)?;
        if existing
            .as_ref()
            .is_some_and(|v| v.phase == VersionPhase::Rejected)
        {
            return Err(conflict("package_version", &p.package_id).into());
        }
        // Retired contents (superseded or uninstalled) proposed again come back with their
        // identity and provenance: unapproved ones wait for the person's exact approval like
        // new ones; approved ones are installed contents again (as an approved version that was
        // never retired is).
        if let Some(v) = existing
            .as_ref()
            .filter(|v| v.phase == VersionPhase::Removed)
        {
            let (phase, waits) = match &v.approval {
                Some(_) => (VersionPhase::Installed, vec![]),
                None => (
                    VersionPhase::Staged,
                    vec!["ExactVersionApproval".to_owned()],
                ),
            };
            tx.packages().update_version(
                &v.package_id,
                &v.manifest_digest,
                phase,
                &waits,
                v.approval.as_ref(),
            )?;
        }
        if existing.is_none() {
            tx.packages().insert_version(&StoredPackageVersion {
                package_id: candidate.package_id.clone(),
                manifest_digest: candidate.manifest_digest.clone(),
                bundle_digest: candidate.bundle_digest.clone(),
                version: candidate.version.clone(),
                publisher_id: candidate.publisher_id.clone(),
                signer_key: candidate.signer_key.clone(),
                inspection: serde_json::to_value(&candidate)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
                provenance: json!({
                    "installedBy": ctx.principal().0.as_str(),
                    "principalKind": kind_name(ctx.principal_kind()),
                    "sessionId": ctx.session_id().map(Id::as_str),
                    "agentAuthored": true,
                    "proposalId": id.as_str(),
                    "source": prep.bundle,
                    "artifactProvenance": prep.provenance,
                }),
                phase: VersionPhase::Staged,
                waits: vec!["ExactVersionApproval".into()],
                approval: None,
                created_at_ms: now,
                updated_at_ms: now,
            })?;
        }
        // A newer proposal supersedes the package's other pending ones: their contents never
        // activate by themselves. They are retired, not rejected (only a person's decision
        // rejects), so a person can still install those exact contents later.
        if let Some(prev) = p
            .pending_digest
            .clone()
            .filter(|d| *d != candidate.manifest_digest)
            && let Some(old) = tx.packages().proposal_for(&p.package_id, &prev)?
            && old.state == "Pending"
        {
            let mut s = old.clone();
            s.state = "Superseded".into();
            s.decided_at_ms = Some(now);
            s.decision = Some(json!({"supersededBy": id.as_str()}));
            tx.packages().update_proposal(&s, old.revision)?;
            tx.packages()
                .update_version(&p.package_id, &prev, VersionPhase::Removed, &[], None)?;
        }
        let approved_before = existing.as_ref().and_then(|v| v.approval.clone());
        p.pending_digest = Some(candidate.manifest_digest.clone());
        Self::save(tx, &mut p, false)?;
        let changes = view::changes(base.as_ref(), &candidate);
        let proposal = StoredProposal {
            id: id.clone(),
            revision: 1,
            package_id: candidate.package_id.clone(),
            manifest_digest: candidate.manifest_digest.clone(),
            bundle_digest: candidate.bundle_digest.clone(),
            version: candidate.version.clone(),
            state: if approved_before.is_some() {
                "Approved".into()
            } else {
                "Pending".into()
            },
            base_package_id: i.base_package_id.as_ref().map(|b| b.as_str().to_owned()),
            originator: ctx.principal().clone(),
            origin: json!({
                "kind": kind_name(ctx.principal_kind()),
                "sessionId": ctx.session_id().map(Id::as_str),
                "projectId": ctx.project_id().map(|p| p.0.as_str()),
                "rootId": ctx.root_id().map(|r| r.0.as_str()),
                "assignmentId": ctx.assignment_id().map(|a| a.0.as_str()),
            }),
            summary: json!({
                "requestedAccess": candidate.access,
                "dependencies": candidate.dependencies,
                "runtimes": candidate.runtimes,
                "contributions": candidate.contribution_ids(),
                "changes": changes,
                "signed": candidate.signer_key.is_some(),
                "source": prep.bundle,
                // The proposal input carries no evidence or rationale members; the reviewer
                // sees the source artifact and its provenance.
                "verificationEvidence": [],
            }),
            decided_by: None,
            decided_at_ms: approved_before.as_ref().map(|_| now),
            decision: approved_before.map(|a| json!({"decision": "Approve", "earlierApproval": a})),
            created_at_ms: now,
        };
        tx.packages().insert_proposal(&proposal)?;
        event(
            tx,
            ctx.principal(),
            &p,
            "PackageProposed",
            json!({"proposalId": id.as_str(), "digest": candidate.manifest_digest, "version": candidate.version}),
        )?;
        self.log_in(
            tx,
            &p.package_id,
            Severity::Info,
            "PackageProposed",
            &format!(
                "Version {} was proposed and waits for a person's approval of its exact contents",
                candidate.version
            ),
            Some(DiagnosticOutcome::Running),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::AddonsPropose {
                value: view::proposal_record(&proposal, p.revision, p.active_digest.is_some()),
            },
        })
    }

    fn result(
        &self,
        tx: &mut dyn WriteTransaction,
        p: &StoredPackage,
        op: &str,
    ) -> Result<OperationResult, StorageError> {
        let read: &dyn PackageRegistryRead = &*tx.packages();
        let record = view::package_record(
            read,
            &self.registry(read)?,
            p,
            &self.platform,
            &*self.runtimes,
            None,
        )?;
        Ok(match op {
            "addons.install" => OperationResult::AddonsInstall { value: record },
            "addons.update" => OperationResult::AddonsUpdate { value: record },
            "addons.enable" => OperationResult::AddonsEnable { value: record },
            "addons.disable" => OperationResult::AddonsDisable { value: record },
            "addons.rollback" => OperationResult::AddonsRollback { value: record },
            "addons.uninstall" => OperationResult::AddonsUninstall { value: record },
            _ => OperationResult::AddonsConfigure { value: record },
        })
    }

    /// Every package with its active version (the dependency graph availability reads).
    pub fn registry(
        &self,
        read: &dyn PackageRegistryRead,
    ) -> Result<view::RegistryView, StorageError> {
        view::load(read)
    }

    // -----------------------------------------------------------------------------------------
    // Host seams (the package host's own units of work)

    /// Retries a staged version of `package_id` whose only remaining waits are quiescence and
    /// compatibility (or that a trusted update admitted), rechecking approval, trust and
    /// compatibility against the current registry and runtime observations immediately before
    /// publishing, in the activation's unit of work: a candidate that would regress stays
    /// pending (waiting for `Compatibility`) and is retried later. The caller holds the
    /// quiescence guard; on success the host announces the publication before releasing it.
    /// A first install (a person approved its proposal) activates as an install. Returns
    /// whether it activated.
    pub fn retry_pending(&self, package_id: &str) -> Result<bool, StorageError> {
        let svc = self.clone_ports();
        let package_id = package_id.to_owned();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut p) = tx.packages().package(&package_id)? else {
                return Ok(false);
            };
            let Some(d) = p.pending_digest.clone() else {
                return Ok(false);
            };
            let Some(v) = tx.packages().package_version(&package_id, &d)? else {
                return Ok(false);
            };
            if v.phase != VersionPhase::Staged || !retryable(&v.waits) {
                return Ok(false);
            }
            // A first install activates only with a recorded approval (nothing to trust yet).
            if !p.installed && v.approval.is_none() {
                return Ok(false);
            }
            // The approval must still be there (trust may have been revoked since staging).
            let approval = match &v.approval {
                Some(a) => Some(a.clone()),
                None => {
                    let current = match &p.active_digest {
                        Some(a) => tx
                            .packages()
                            .package_version(&package_id, a)?
                            .as_ref()
                            .and_then(policy::inspection),
                        None => None,
                    };
                    let (Some(c), Some(next), Some(trust)) = (
                        current,
                        policy::inspection(&v),
                        p.trust.as_ref().and_then(policy::Trust::from_record),
                    ) else {
                        return Ok(false);
                    };
                    if v.provenance["agentAuthored"].as_bool() == Some(true)
                        || policy::auto_update(&trust, &c, &next).is_err()
                    {
                        return Ok(false);
                    }
                    Some(
                        json!({"kind": "TrustedPublisher", "publisherId": trust.publisher_id, "signerKey": trust.signer_key, "atMs": tx.now_ms()}),
                    )
                }
            };
            // Compatibility against the registry and runtimes as they are now.
            let regression = svc.regresses(tx, &p, &v)?;
            if let Some(why) = regression {
                if v.waits != ["Compatibility"] {
                    tx.packages().update_version(
                        &package_id,
                        &d,
                        VersionPhase::Staged,
                        &["Compatibility".to_owned()],
                        v.approval.as_ref(),
                    )?;
                    let host = tx.store().host_actor.clone();
                    event(
                        tx,
                        &host,
                        &p,
                        "PackageStaged",
                        json!({"digest": d, "version": v.version, "waits": ["Compatibility"], "reason": why}),
                    )?;
                    svc.log_in(
                        tx,
                        &package_id,
                        Severity::Info,
                        "PackageStaged",
                        &format!(
                            "Version {} stays pending: activating it now would regress ({why})",
                            v.version
                        ),
                        Some(DiagnosticOutcome::Running),
                    )?;
                }
                return Ok(false);
            }
            let mut v = v;
            v.approval = approval;
            let host = tx.store().host_actor.clone();
            let cause = if p.installed {
                "QuiescentRetry"
            } else {
                p.installed = true;
                p.enabled = true;
                p.failure = None;
                "ApprovedInstall"
            };
            svc.activate(tx, &host, &mut p, &v, cause)?;
            svc.observe(tx, cause)?;
            Ok(true)
        })
    }

    /// Records that the helper of the active version of `package_id` failed to start (or
    /// failed repeatedly). When `admitted_nothing` (no invocation of the new generation was
    /// admitted) and the active version replaced a predecessor whose state is still compatible,
    /// the host's guarded automatic rollback restores the predecessor in the same unit of work.
    /// Returns whether it rolled back.
    pub fn startup_failed(
        &self,
        package_id: &str,
        digest: &str,
        code: &str,
        message: &str,
        attempts: i64,
        admitted_nothing: bool,
    ) -> Result<bool, StorageError> {
        let svc = self.clone_ports();
        let (package_id, digest, code, message) = (
            package_id.to_owned(),
            digest.to_owned(),
            code.to_owned(),
            message.to_owned(),
        );
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut p) = tx.packages().package(&package_id)? else {
                return Ok(false);
            };
            if p.active_digest.as_deref() != Some(digest.as_str()) {
                return Ok(false);
            }
            let host = tx.store().host_actor.clone();
            // Never back into a version whose own startup failure is recorded (it just exhausted
            // its budget, possibly under another state generation): no viable predecessor means
            // the failure is recorded durably instead.
            if admitted_nothing
                && let Some(pred) = p.predecessor_digest.clone()
                && failure_of(&p, &pred).is_none()
                && let Some(v) = tx.packages().package_version(&package_id, &pred)?
                && v.phase == VersionPhase::Installed
                && v.approval.is_some()
                && policy::inspection(&v)
                    .is_some_and(|pi| policy::static_incompatibility(&pi, &svc.platform).is_none())
                && svc.regresses(tx, &p, &v)?.is_none()
            {
                let failed = digest.clone();
                let sp = tx.savepoint()?;
                match svc.restore(tx, &host, &mut p, &v, "StartupRollback") {
                    Ok(true) => {
                        tx.release(sp)?;
                        // The failed version stays installed with its failure recorded, never
                        // active again by itself (automatic rollback never returns to it).
                        p.failure = Some(
                            json!({"code": code, "message": message, "atMs": tx.now_ms(),
                            "attempts": attempts, "digest": failed, "rolledBack": true}),
                        );
                        Self::save(tx, &mut p, false)?;
                        svc.log_in(tx, &package_id, Severity::Warning, "PackageRolledBack", &format!("The new version failed to start ({code}: {message}) before any work was admitted; the previous version {} was restored", v.version), Some(DiagnosticOutcome::Failed))?;
                        event(
                            tx,
                            &host,
                            &p,
                            "PackageStartupRolledBack",
                            json!({"failedDigest": failed, "restoredDigest": pred, "code": code}),
                        )?;
                        svc.observe(tx, "StartupRollback")?;
                        return Ok(true);
                    }
                    Ok(false) => tx.rollback_to(sp)?,
                    Err(e) => {
                        tx.rollback_to(sp)?;
                        return Err(e);
                    }
                }
            }
            p.failure = Some(
                json!({"code": code, "message": message, "atMs": tx.now_ms(), "attempts": attempts, "digest": digest}),
            );
            Self::save(tx, &mut p, false)?;
            event(
                tx,
                &host,
                &p,
                "PackageFailed",
                json!({"digest": digest, "code": code, "attempts": attempts}),
            )?;
            svc.log_in(
                tx,
                &package_id,
                Severity::Error,
                &code,
                &message,
                Some(DiagnosticOutcome::Failed),
            )?;
            svc.observe(tx, "StartupFailed")?;
            Ok(false)
        })
    }

    /// Records that the helper of `digest` is running and ready (clears an earlier failure of
    /// it) and attributes it in history.
    pub fn record_running(
        &self,
        package_id: &str,
        digest: &str,
        process: &Id,
    ) -> Result<(), StorageError> {
        let svc = self.clone_ports();
        let (package_id, digest, process) =
            (package_id.to_owned(), digest.to_owned(), process.clone());
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut p) = tx.packages().package(&package_id)? else {
                return Ok(());
            };
            if p.active_digest.as_deref() != Some(digest.as_str()) {
                return Ok(());
            }
            let host = tx.store().host_actor.clone();
            if failure_of(&p, &digest).is_some() {
                p.failure = None;
                Self::save(tx, &mut p, false)?;
            }
            event(
                tx,
                &host,
                &p,
                "PackageHelperReady",
                json!({"digest": digest, "processId": process.as_str()}),
            )?;
            svc.log_in(
                tx,
                &package_id,
                Severity::Info,
                "PackageHelperReady",
                "The helper process is ready",
                Some(DiagnosticOutcome::Succeeded),
            )?;
            svc.observe(tx, "HelperReady").map(|_| ())
        })
    }

    /// Records helper output and process events (bounded and redacted by the diagnostics
    /// service) for `addons.logs`.
    pub fn record_log(
        &self,
        package_id: &str,
        severity: Severity,
        code: &str,
        message: &str,
        output: Option<String>,
    ) {
        let mut e = LogEntry::new(COMPONENT, code, severity, message);
        e.target_kind = Some(TARGET_KIND.into());
        e.target_id = Id::new(package_id).ok();
        e.output = output;
        let _ = self.diagnostics.log(e);
    }

    /// Removes install directories nothing needs: versions of uninstalled packages, versions
    /// older than the rollback predecessor that no pending or running instance pins
    /// (`in_use`), and unreferenced directories (an interrupted publication) older than
    /// `orphan_age` (`None`: any age, only before admission at boot). Records every removal.
    pub fn collect_files(
        &self,
        in_use: &BTreeSet<(String, String)>,
        orphan_age: Option<Duration>,
    ) -> Result<usize, StorageError> {
        // The whole pass (eligibility in its unit of work and the removals after it) excludes
        // publication, so a republish that reuses a directory never loses its files.
        let _gate = self.files_gate.lock().unwrap_or_else(|p| p.into_inner());
        // A publication in progress owns its staging directory: outside boot only abandoned
        // (old) ones are removed.
        let _ = self.files.clean_staging(orphan_age);
        let on_disk = self
            .files
            .installed()
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        let svc = self.clone_ports();
        let mut keep = in_use.clone();
        // Directories a command published and has not settled are never collected.
        keep.extend(
            self.publishing
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .keys()
                .cloned(),
        );
        let candidates = on_disk.clone();
        let removable: Vec<(String, String, bool)> =
            run_write(&*self.store, WriteOptions::default(), move |tx| {
                let mut out = Vec::new();
                for (pkg, digest) in candidates {
                    if keep.contains(&(pkg.clone(), digest.clone())) {
                        continue;
                    }
                    let p = tx.packages().package(&pkg)?;
                    let v = tx.packages().package_version(&pkg, &digest)?;
                    let Some(v) = v else {
                        out.push((pkg, digest, true));
                        continue;
                    };
                    let pinned = p.as_ref().is_some_and(|p| {
                        p.installed
                            && [&p.active_digest, &p.predecessor_digest, &p.pending_digest]
                                .iter()
                                .any(|d| d.as_deref() == Some(digest.as_str()))
                    });
                    let staged = v.phase == VersionPhase::Staged;
                    if !pinned && v.phase == VersionPhase::Rejected {
                        // A rejected version's files go; the rejection itself stays recorded.
                        out.push((pkg, digest, false));
                    } else if !pinned && !staged && v.phase != VersionPhase::Removed {
                        tx.packages().update_version(
                            &pkg,
                            &digest,
                            VersionPhase::Removed,
                            &[],
                            v.approval.as_ref(),
                        )?;
                        if let Some(p) = &p {
                            let host = tx.store().host_actor.clone();
                            event(
                                tx,
                                &host,
                                p,
                                "PackageFilesRemoved",
                                json!({"digest": digest, "version": v.version}),
                            )?;
                        }
                        out.push((pkg, digest, false));
                    } else if v.phase == VersionPhase::Removed {
                        out.push((pkg, digest, false));
                    }
                }
                Ok(out)
            })?;
        let mut removed = 0;
        for (pkg, digest, orphan) in removable {
            if orphan {
                let old_enough = match orphan_age {
                    None => true,
                    Some(age) => svc.files.idle_for(&pkg, &digest).is_some_and(|e| e >= age),
                };
                if !old_enough {
                    continue;
                }
            }
            if self.files.remove(&pkg, &digest).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Records a person's decision on a proposal (the seam for DW-14's
    /// `approvals.record_human_decision`). Approve records the exact-version approval of the
    /// proposed contents (activation follows through `addons.install`/`.update` or, for a
    /// staged update waiting only on approval, at once); Reject leaves the active version
    /// unchanged and the contents never activate. Only a person may decide.
    pub fn decide_proposal(
        &self,
        ctx: &InvocationContext,
        proposal_id: &Id,
        expected_revision: i64,
        manifest_digest: &str,
        approve: bool,
        rationale: &str,
    ) -> Result<Result<StoredProposal, Refusal>, StorageError> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Ok(Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_proposal",
                Some(proposal_id.clone()),
            )));
        }
        let (ctx, proposal_id, digest, rationale) = (
            ctx.clone(),
            proposal_id.clone(),
            manifest_digest.to_owned(),
            rationale.chars().take(4096).collect::<String>(),
        );
        let svc = self.clone_ports();
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(pr) = tx.packages().proposal(&proposal_id)? else {
                return Ok(Err(not_found("package_proposal", proposal_id.as_str())));
            };
            if pr.revision != expected_revision {
                return Ok(Err(Refusal::new(
                    DomainErrorCode::RevisionConflict,
                    "package_proposal",
                    Some(proposal_id.clone()),
                )));
            }
            if pr.manifest_digest != digest {
                return Ok(Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "package_proposal",
                    Some(proposal_id.clone()),
                )));
            }
            if pr.state != "Pending" {
                return Ok(Err(conflict("package_proposal", proposal_id.as_str())));
            }
            let now = tx.now_ms();
            let mut d = pr.clone();
            d.state = if approve {
                "Approved".into()
            } else {
                "Rejected".into()
            };
            d.decided_by = Some(ctx.principal().clone());
            d.decided_at_ms = Some(now);
            d.decision = Some(
                json!({"decision": if approve { "Approve" } else { "Reject" }, "rationale": rationale}),
            );
            let d = tx.packages().update_proposal(&d, pr.revision)?;
            let Some(mut p) = tx.packages().package(&pr.package_id)? else {
                return Ok(Ok(d));
            };
            if approve {
                let approval = json!({"kind": "Person", "by": ctx.principal().0.as_str(), "atMs": now, "proposalId": pr.id.as_str()});
                if let Some(v) = tx
                    .packages()
                    .package_version(&pr.package_id, &pr.manifest_digest)?
                {
                    let mut waits: Vec<String> = v
                        .waits
                        .iter()
                        .filter(|w| {
                            !matches!(
                                w.as_str(),
                                "ExactVersionApproval" | "TrustApproval" | "AccessApproval"
                            )
                        })
                        .cloned()
                        .collect();
                    // Nothing else to wait for: the host activates the pending version at its
                    // next quiescent boundary (rechecking compatibility then).
                    if waits.is_empty()
                        && v.phase == VersionPhase::Staged
                        && p.pending_digest.as_deref() == Some(pr.manifest_digest.as_str())
                    {
                        waits.push("Quiescence".into());
                    }
                    tx.packages().update_version(
                        &pr.package_id,
                        &pr.manifest_digest,
                        v.phase,
                        &waits,
                        Some(&approval),
                    )?;
                }
                event(
                    tx,
                    ctx.principal(),
                    &p,
                    "PackageProposalApproved",
                    json!({"proposalId": pr.id.as_str(), "digest": pr.manifest_digest}),
                )?;
            } else {
                tx.packages().update_version(
                    &pr.package_id,
                    &pr.manifest_digest,
                    VersionPhase::Rejected,
                    &[],
                    None,
                )?;
                if p.pending_digest.as_deref() == Some(pr.manifest_digest.as_str()) {
                    p.pending_digest = None;
                    Self::save(tx, &mut p, false)?;
                }
                event(
                    tx,
                    ctx.principal(),
                    &p,
                    "PackageProposalRejected",
                    json!({"proposalId": pr.id.as_str(), "digest": pr.manifest_digest}),
                )?;
                svc.log_in(
                    tx,
                    &pr.package_id,
                    Severity::Info,
                    "PackageProposalRejected",
                    &format!(
                        "Proposed version {} was rejected; the active version is unchanged",
                        pr.version
                    ),
                    None,
                )?;
            }
            Ok(Ok(d))
        })
    }

    /// A cheap clone of the ports for use inside a write job.
    fn clone_ports(&self) -> Arc<PackageService> {
        Arc::new(PackageService {
            store: self.store.clone(),
            ids: self.ids.clone(),
            artifacts: self.artifacts.clone(),
            inspector: self.inspector.clone(),
            files: self.files.clone(),
            runtimes: self.runtimes.clone(),
            platform: self.platform.clone(),
            diagnostics: self.diagnostics.clone(),
            activity: RwLock::new(self.activity()),
            validator: self.validator.clone(),
            consumers: RwLock::new(self.consumers()),
            publishing: self.publishing.clone(),
            files_gate: self.files_gate.clone(),
            quiesce_wait: self.quiesce_wait,
        })
    }

    /// A configuration value refused by its schema, with field-level errors in the diagnostic
    /// the fault names.
    fn config_refused(
        tx: &mut dyn WriteTransaction,
        package: &str,
        errors: Vec<FieldError>,
        cell: Option<&ConfigRefusal>,
    ) -> DomainDecision {
        let diagnostic = tx.new_id(IdKind::Diagnostic);
        let mut fault = Refusal::new(
            DomainErrorCode::InvalidArgument,
            "package_configuration",
            Id::new(package).ok(),
        )
        .fault(CorrelationId(tx.new_id(IdKind::Correlation)));
        fault.diagnostic_id = Some(diagnostic.clone());
        if let Some(c) = cell {
            *c.0.lock().unwrap_or_else(|p| p.into_inner()) =
                Some((diagnostic, package.to_owned(), errors));
        }
        DomainDecision::Refused { fault }
    }
}

/// The recorded failure of `digest` (a package keeps one failure record, of the version that
/// last failed; it marks the active version unavailable only while it is that version).
pub(crate) fn failure_of<'a>(p: &'a StoredPackage, digest: &str) -> Option<&'a Value> {
    p.failure
        .as_ref()
        .filter(|f| f["digest"].as_str() == Some(digest))
}

/// Whether a staged version's remaining waits are ones the host clears by itself (quiescence,
/// and compatibility rechecked at every retry).
pub fn retryable(waits: &[String]) -> bool {
    !waits.is_empty()
        && waits
            .iter()
            .all(|w| w == "Quiescence" || w == "Compatibility")
}

fn update_op(update: bool) -> &'static str {
    if update {
        "addons.update"
    } else {
        "addons.install"
    }
}

/// The selected lifecycle command, checked against the host-selected operation.
fn lifecycle(input: &OperationInput) -> Option<(&'static str, &AddonCommand)> {
    let OperationInput::Addons(i) = input else {
        return None;
    };
    let (op, req) = match i {
        ops::Input::Install(r) => ("addons.install", r),
        ops::Input::Update(r) => ("addons.update", r),
        ops::Input::Enable(r) => ("addons.enable", r),
        ops::Input::Disable(r) => ("addons.disable", r),
        ops::Input::Rollback(r) => ("addons.rollback", r),
        ops::Input::Uninstall(r) => ("addons.uninstall", r),
        _ => return None,
    };
    let matches = matches!(
        (op, &req.command),
        ("addons.install", AddonCommand::Install { .. })
            | ("addons.update", AddonCommand::Update { .. })
            | ("addons.enable", AddonCommand::Enable { .. })
            | ("addons.disable", AddonCommand::Disable { .. })
            | ("addons.rollback", AddonCommand::Rollback { .. })
            | ("addons.uninstall", AddonCommand::Uninstall { .. })
    );
    matches.then_some((op, &req.command))
}

impl QueryHandler for PackageService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answered = match query.input() {
            OperationInput::Addons(ops::Input::Catalog(i)) => {
                view::catalog(snapshot, ctx, i, &self.platform, &*self.runtimes)
            }
            OperationInput::Addons(ops::Input::Logs(i)) => view::logs(snapshot, ctx, i),
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let registry = snapshot.packages().registry_revision().unwrap_or(1);
        let result = answered.map_err(|f| query_fault(f, "package", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(registry)),
            result,
        })
    }
}

impl CommandHandler for PackageService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        // A replay decides nothing new, so nothing is staged for it.
        if env.read(|s| Ok(s.receipts().lookup(command.identity())?.is_some()))? {
            return Ok(None);
        }
        if let OperationInput::Addons(ops::Input::Propose(i)) = command.input() {
            let prep = self.prepare_bundle(ctx, &i.source, None);
            return Ok(Some(Prepared::new(Prep::Bundle(Box::new(prep)))));
        }
        let Some((_, cmd)) = lifecycle(command.input()) else {
            // addons.configure: fence the package's admissions while the change commits.
            return Ok(Some(Prepared::new(match command.input() {
                OperationInput::Addons(ops::Input::Configure(i)) => Prep::Configure(
                    GuardCell::new(Some(self.activity().fence(i.package_id.as_str()))),
                    ConfigRefusal::default(),
                ),
                _ => Prep::Lifecycle(GuardCell::new(None)),
            })));
        };
        Ok(Some(Prepared::new(match cmd {
            AddonCommand::Install { bundle, .. } => {
                Prep::Bundle(Box::new(self.prepare_bundle(ctx, bundle, None)))
            }
            AddonCommand::Update {
                package_id, bundle, ..
            } => Prep::Bundle(Box::new(self.prepare_bundle(
                ctx,
                bundle,
                Some(package_id.as_str()),
            ))),
            AddonCommand::Rollback { package_id, .. } => Prep::Lifecycle(GuardCell::new(
                self.activity()
                    .quiesce(package_id.as_str(), self.quiesce_wait),
            )),
            AddonCommand::Enable { package_id, .. }
            | AddonCommand::Disable { package_id, .. }
            | AddonCommand::Uninstall { package_id, .. } => Prep::Lifecycle(GuardCell::new(Some(
                self.activity().fence(package_id.as_str()),
            ))),
        })))
    }

    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        // The registry change is announced before its gate lifts, so an admission that read the
        // registry before the commit reads it again instead of reaching the retiring version.
        self.activity().published();
        match prepared.and_then(|p| p.get::<Prep>()) {
            Some(Prep::Bundle(b)) => {
                b.guard.release();
                // A refused bundle's reason is recorded once, after the refusal (whose own
                // writes the gateway undoes); a replay prepares nothing and logs nothing.
                let refused = matches!(
                    reply,
                    Some(crate::protocol::CommandReply::Recorded { receipt, .. })
                        if matches!(receipt.decision, DomainDecision::Refused { .. })
                );
                if let (true, Err((code, detail))) = (refused, &b.outcome) {
                    self.record_log(
                        b.target.as_deref().unwrap_or("bundle"),
                        Severity::Error,
                        code,
                        &format!("The bundle was refused: {detail}"),
                        None,
                    );
                }
            }
            Some(Prep::Lifecycle(g)) => g.release(),
            Some(Prep::Configure(g, refusal)) => {
                g.release();
                let refused = matches!(
                    reply,
                    Some(crate::protocol::CommandReply::Recorded { receipt, .. })
                        if matches!(receipt.decision, DomainDecision::Refused { .. })
                );
                let detail = refusal.0.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let (true, Some((id, package, errors))) = (refused, detail) {
                    let lines: Vec<String> = errors
                        .iter()
                        .map(|e| {
                            let at = if e.pointer.is_empty() {
                                "/"
                            } else {
                                e.pointer.as_str()
                            };
                            format!("{at}: {}", e.message)
                        })
                        .collect();
                    let mut e = LogEntry::new(
                        COMPONENT,
                        "ConfigurationInvalid",
                        Severity::Error,
                        format!(
                            "The configuration was refused by the package's schema:\n{}",
                            lines.join("\n")
                        ),
                    );
                    e.target_kind = Some(TARGET_KIND.into());
                    e.target_id = Id::new(&package).ok();
                    e.outcome = Some(DiagnosticOutcome::Failed);
                    e.error_class = Some(DiagnosticErrorClass::InvalidInput);
                    let _ = self.diagnostics.log_with_id(id, e);
                }
            }
            None => {}
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let prep = command.prepared::<Prep>();
        let bundle = match prep {
            Some(Prep::Bundle(b)) => Some(b.as_ref()),
            _ => None,
        };
        let guard = match prep {
            Some(Prep::Lifecycle(g)) => Some(g),
            _ => None,
        };
        let refusal = match prep {
            Some(Prep::Configure(_, r)) => Some(r),
            _ => None,
        };
        let sp = tx.savepoint()?;
        let outcome = match command.input() {
            OperationInput::Addons(ops::Input::Configure(i)) => self.configure(tx, ctx, i, refusal),
            OperationInput::Addons(ops::Input::Propose(i)) => self.propose(tx, ctx, i, bundle),
            input => match lifecycle(input) {
                Some((
                    _,
                    AddonCommand::Install {
                        expected_registry_revision,
                        ..
                    },
                )) => self.install_or_update(
                    tx,
                    ctx,
                    bundle,
                    None,
                    expected_registry_revision.get(),
                    false,
                ),
                Some((
                    _,
                    AddonCommand::Update {
                        package_id,
                        expected_registry_revision,
                        ..
                    },
                )) => self.install_or_update(
                    tx,
                    ctx,
                    bundle,
                    Some(package_id),
                    expected_registry_revision.get(),
                    true,
                ),
                Some((
                    _,
                    AddonCommand::Enable {
                        package_id,
                        expected_registry_revision,
                    },
                )) => self.enable(tx, ctx, package_id, expected_registry_revision.get()),
                Some((
                    _,
                    AddonCommand::Disable {
                        package_id,
                        expected_registry_revision,
                    },
                )) => self.disable(tx, ctx, package_id, expected_registry_revision.get()),
                Some((
                    _,
                    AddonCommand::Uninstall {
                        package_id,
                        expected_registry_revision,
                    },
                )) => self.uninstall(tx, ctx, package_id, expected_registry_revision.get()),
                Some((
                    _,
                    AddonCommand::Rollback {
                        package_id,
                        target_manifest_digest,
                        expected_registry_revision,
                        expected_state_generation,
                    },
                )) => self.rollback(
                    tx,
                    ctx,
                    package_id,
                    &String::from(target_manifest_digest.clone()),
                    expected_registry_revision.get(),
                    expected_state_generation.get(),
                    guard,
                ),
                _ => Err(
                    Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into(),
                ),
            },
        };
        // The gateway undoes every write of a refused decision; this savepoint also undoes the
        // writes before a storage failure the use case reports.
        let outcome = match outcome {
            Ok(d) => {
                // Every effective availability or active-version change this command made is
                // announced in its own unit of work (ADDON-04).
                if matches!(d, DomainDecision::Applied { .. }) {
                    self.observe(tx, command.descriptor().name)?;
                }
                tx.release(sp)?;
                Ok(d)
            }
            Err(Failure::Storage(e)) => {
                tx.rollback_to(sp)?;
                return Err(HandlerFailure::Storage(e));
            }
            Err(r) => {
                tx.rollback_to(sp)?;
                Err(r)
            }
        };
        decided(tx, outcome)
    }
}

/// `true` when `e` is a revision conflict of the package registry (the caller re-reads).
pub fn is_registry_conflict(e: &StorageError) -> bool {
    matches!(e, StorageError::RevisionConflict(c) if c.target_kind == "package_registry")
}

#[doc(hidden)]
pub fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<PackageService>();
}

/// For callers composing an [`InspectError`] into a refusal code.
pub fn inspect_code(e: &InspectError) -> &'static str {
    e.code()
}
