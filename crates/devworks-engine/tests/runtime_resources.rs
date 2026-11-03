//! The boot entry point with owned processes, resource leases and keep-awake on the real SQLite
//! store: an earlier boot's orphaned tree and the physical lease it served are recovered before
//! the runtime admits anything, keep-awake follows the setting through `settings.save_revision`,
//! and shutdown stops owned trees and releases keep-awake. Trivial OS commands only; this binary
//! holds the only test that spawns processes so it never overlaps a concurrent store reopen.
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::paths::FsPathProbe;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::HostRoles;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::process::{
    PowerAssertion, PowerPort, PowerRequest, Probe, ProcessHost, ProcessSpec,
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{ActorId, CommandReply};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::power::{KEEP_AWAKE_KEY, KeepAwakeStatus};
use devworks_core::services::process::{LaunchPolicy, LaunchRequest, StopOutcome};
use devworks_core::services::resources::{
    MachineResource, ResourceService, desktop, register_machine,
};
use devworks_core::settings::catalog::descriptor;
use devworks_core::settings::model;
use devworks_core::settings::service::SettingsService;
use devworks_core::settings::view::Health;
use devworks_engine::{Composition, Runtime, RuntimeConfig, boot};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Counts the OS power requests held.
#[derive(Default)]
struct FakePower(Arc<AtomicUsize>);
struct Held(Arc<AtomicUsize>);
impl PowerAssertion for Held {
    fn status(&mut self) -> PowerRequest {
        PowerRequest::Effective { mechanism: "fake" }
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl PowerPort for FakePower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Held(self.0.clone())))
    }
}

fn gpu() -> MachineResource {
    MachineResource {
        name: "gpu".into(),
        capacity: 1,
        physical: true,
        labels: vec![],
    }
}
/// A root and a descendant that each sleep about 30 seconds.
fn sleeping_tree() -> ProcessSpec {
    let argv: Vec<String> = if cfg!(windows) {
        ["cmd", "/C", "ping", "-n", "30", "127.0.0.1"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 30 & sleep 30; wait".into(),
        ]
    };
    ProcessSpec::new(&argv).unwrap()
}
/// Writes `power.keepAwake` directly (as an earlier boot's save would have).
fn set_keep_awake(store: &dyn ApplicationStore, on: bool) {
    write(store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let schema =
            serde_json::to_value(descriptor(KEEP_AWAKE_KEY).unwrap().schema_ref()).unwrap();
        let a = serde_json::from_value(
            json!({"key": KEEP_AWAKE_KEY, "value": {"schema": schema, "value": on}}),
        )
        .unwrap();
        let plan = model::plan(&current, &[a]);
        let body = model::body(
            &plan,
            &ActorId(id("host")),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}
/// Saves `power.keepAwake` through the runtime's settings command.
fn save_keep_awake(app: &Runtime, user: &InvocationContext, key: &str, on: bool) {
    let revision = read(&**app.store(), |s| {
        model::current_layer(s.configuration(), &ConfigScope::Workspace)
    })
    .revision;
    let schema = serde_json::to_value(descriptor(KEEP_AWAKE_KEY).unwrap().schema_ref()).unwrap();
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
        "body": {"kind": "settings.save_revision", "scope": {"kind": "Workspace"},
            "expectedRevision": revision.to_string(),
            "entries": [{"key": KEEP_AWAKE_KEY, "value": {"schema": schema, "value": on}}]}});
    match app
        .command(user, "settings.save_revision", frame.to_string().as_bytes())
        .expect("addressable")
    {
        CommandReply::Recorded { receipt, .. } => assert!(
            matches!(receipt.decision, DomainDecision::Applied { .. }),
            "{:?}",
            receipt.decision
        ),
        other => panic!("{other:?}"),
    }
}

fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    power: Arc<FakePower>,
) -> Composition {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    config.collect_interval = Duration::from_secs(3600);
    config.machine_resources = vec![desktop(), gpu()];
    config.shutdown_grace = Duration::from_millis(500);
    config.shutdown_bound = Duration::from_secs(10);
    Composition {
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(Authority),
        artifacts,
        resources: Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        process_host: Arc::new(NativeProcessHost),
        power,
        use_cases: UseCases::new().with_settings(Arc::new(SettingsService {
            roles: Arc::new(HostRoles { store }),
            paths: Arc::new(FsPathProbe),
            ids: h.ids.clone(),
        })),
        consumers: Vec::new(),
        config,
    }
}

#[test]
fn boot_recovers_processes_and_leases_before_admission_and_shutdown_releases_them() {
    let h = Harness::new();
    let rt = runtime();
    // An earlier boot: the physical gpu is leased to a tree that is still running when that
    // boot dies, and keep-awake is on.
    let store = h.open();
    let tree = NativeProcessHost.spawn(&sleeping_tree()).unwrap();
    let identity = tree.identity().clone();
    std::mem::forget(tree);
    let (platform, ident) = (NativeProcessHost.platform().to_owned(), identity.clone());
    let boot_id = store.info().boot_id.unwrap();
    write(&*store, move |tx| {
        let res = register_machine(tx, &gpu())?;
        let fence = tx.resources().next_fence(&res.id)?;
        tx.resources().insert_lease(&StoredLease {
            id: id("lease-gpu"),
            resource_id: res.id,
            phase: LeasePhase::Held,
            end_reason: None,
            owner: ActorId(id("host")),
            holder_key: "actor:host".into(),
            session_id: None,
            project_id: None,
            root_id: None,
            assignment_id: None,
            boot_id,
            purpose: Some("earlier boot".into()),
            priority: 1,
            units: 1,
            lease_ms: 3_600_000,
            queue_seq: 1,
            fence,
            requested_at_ms: T0,
            granted_at_ms: Some(T0),
            renewed_at_ms: None,
            expires_at_ms: T0 + 3_600_000,
            ended_at_ms: None,
            unreachable_since_ms: None,
            revision: 1,
        })?;
        let mut p = tx.resources().insert_process(NewProcess {
            id: id("process-orphan"),
            platform,
            program: "sh".into(),
            argv_sha256: "a".repeat(64),
            operation_id: None,
            lease_id: Some(id("lease-gpu")),
            actor: ActorId(id("host")),
            session_id: None,
            project_id: None,
            root_id: None,
            assignment_id: None,
        })?;
        p.phase = ProcessPhase::Running;
        p.pid = Some(i64::from(ident.pid));
        p.birth = Some(ident.birth.clone());
        p.group = ident.group;
        p.revision = 2;
        tx.resources().update_process(&p, 1)
    })
    .unwrap();
    set_keep_awake(&*store, true);
    drop(store);

    let store = h.open();
    assert_eq!(
        NativeProcessHost.probe(&identity),
        Probe::Alive,
        "still running at reopen"
    );
    let power = Arc::new(FakePower::default());
    let app = rt
        .block_on(boot(composition(&h, store.clone(), power.clone())))
        .unwrap();
    // The runtime is the only admission handle; by the time boot returned it, the earlier
    // boot's tree was killed and its lease transferred.
    let report = app.report().resources.clone();
    assert_eq!(
        (report.machine_resources, report.recovery.killed),
        (2, 1),
        "{report:?}"
    );
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    let process = read(&*store, |s| s.resources().process(&id("process-orphan"))).unwrap();
    assert_eq!(process.phase, ProcessPhase::Reaped);
    let lease = read(&*store, |s| s.resources().lease(&id("lease-gpu"))).unwrap();
    assert_eq!(lease.end_reason, Some(LeaseEndReason::Reconciled));
    let events = read(&*store, |s| s.history().page(&EventFilter::All, 0, 500)).events;
    let boot_pass = events
        .iter()
        .find(|e| e.kind == "ResourcesReconciled" && e.payload["cause"] == "Boot")
        .expect("boot reconciliation recorded")
        .sequence;
    // The first admitted command is granted the transferred gpu with the next fence.
    let user = principal(&*store, "user-a", PrincipalKind::Human);
    let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": "acquire-1",
        "schemaRevision": 2, "body": {"kind": "resources.acquire", "resourceId": "gpu",
            "expectedRevision": "1", "units": 1, "leaseMs": "60000", "purpose": "after boot"}});
    let granted = match app
        .command(&user, "resources.acquire", frame.to_string().as_bytes())
        .expect("addressable")
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::ResourcesAcquire { value },
            } => value,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(
        granted.fence.get(),
        2,
        "granted after the earlier owner was stopped"
    );
    let decided = read(&*store, |s| s.history().page(&EventFilter::All, 0, 500))
        .events
        .into_iter()
        .find(|e| e.kind == "CommandDecided")
        .expect("decision recorded")
        .sequence;
    assert!(
        boot_pass < decided,
        "recovery precedes the first admitted command"
    );

    // Keep-awake was applied at boot and follows settings.save_revision.
    assert_eq!(
        app.report().keep_awake,
        KeepAwakeStatus::Effective { mechanism: "fake" }
    );
    assert_eq!(power.0.load(Ordering::SeqCst), 1);
    assert_eq!(app.keep_awake_health(), Health::Available);
    save_keep_awake(&app, &user, "settings-1", false);
    assert_eq!(
        power.0.load(Ordering::SeqCst),
        0,
        "disabling releases the request"
    );
    assert_eq!(app.keep_awake(), KeepAwakeStatus::Off);
    save_keep_awake(&app, &user, "settings-2", true);
    assert_eq!(power.0.load(Ordering::SeqCst), 1);

    // Shutdown stops the owned tree within its bound and releases keep-awake.
    let fs = NativeFilesystem::new();
    let roots = ProtectedRoots::new(vec![], vec![], None);
    let policy = LaunchPolicy {
        fs: &fs,
        roots: &roots,
        protected_branches: &[],
    };
    let owned = app
        .processes()
        .launch(
            &user,
            LaunchRequest {
                spec: sleeping_tree(),
                operation_id: None,
                lease: None,
                approved_hold: false,
            },
            &policy,
        )
        .expect("launched");
    assert_eq!(NativeProcessHost.probe(&owned.identity), Probe::Alive);
    let stopped = rt.block_on(app.shutdown()).into_result().unwrap();
    assert_eq!(stopped.len(), 1);
    assert!(
        matches!(stopped[0].1, StopOutcome::Reaped { .. }),
        "{stopped:?}"
    );
    assert_eq!(NativeProcessHost.probe(&owned.identity), Probe::Gone);
    assert_eq!(
        power.0.load(Ordering::SeqCst),
        0,
        "shutdown releases keep-awake"
    );
    let last = read(&*store, |s| s.history().page(&EventFilter::All, 0, 500))
        .events
        .into_iter()
        .rfind(|e| e.kind == "ResourcesReconciled")
        .unwrap();
    assert_eq!(last.payload["cause"], "Shutdown");
}
