//! Lifecycle housekeeping of resources and owned processes on the real SQLite store: boot
//! registration and recovery of an earlier boot's launch, a sweep transferring an expired
//! physical lease whose owner is quiescent, sleep/wake detection and shutdown. No inference.
mod common;

use common::*;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::ports::process::ProcessHost;
use devworks_core::ports::storage::*;
use devworks_core::protocol::ActorId;
use devworks_core::services::process::ProcessSupervisor;
use devworks_core::services::resources::{DESKTOP, ReconcileCause, ResourceService, desktop};
use devworks_engine::resources::Housekeeper;
use std::sync::Arc;
use std::time::Duration;

fn housekeeper(h: &Harness, store: &Arc<dyn ApplicationStore>) -> Housekeeper {
    let resources = Arc::new(ResourceService::new(h.ids.clone(), h.clock()));
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        resources.clone(),
    ));
    Housekeeper::new(store.clone(), h.clock(), resources, supervisor)
}
fn events(store: &dyn ApplicationStore, kind: &str) -> Vec<EventRecord> {
    read(store, |r| r.history().page(&EventFilter::All, 0, 500))
        .events
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

#[test]
fn boot_sweep_wake_and_shutdown_housekeeping() {
    let h = Harness::new();
    let store = h.open();
    // An earlier boot recorded a launch but never its identity.
    write(&*store, |tx| {
        tx.resources().insert_process(NewProcess {
            id: id("process-lost"),
            platform: NativeProcessHost.platform().into(),
            program: "sh".into(),
            argv_sha256: "c".repeat(64),
            operation_id: None,
            lease_id: None,
            actor: ActorId(id("host")),
            session_id: None,
            project_id: None,
            root_id: None,
            assignment_id: None,
        })
    })
    .unwrap();
    drop(store);
    let store = h.open();
    let keeper = housekeeper(&h, &store);
    let rt = runtime();
    let boot = rt.block_on(keeper.boot(vec![desktop()])).unwrap();
    assert_eq!((boot.machine_resources, boot.recovery.lost), (1, 1));
    let desk = read(&*store, |r| {
        r.resources()
            .resource_by_key(&canonical_key(ResourceScopeKind::Machine, None, DESKTOP))
    })
    .expect("desktop registered");
    assert_eq!(
        (desk.name.as_str(), desk.capacity, desk.physical),
        (DESKTOP, 1, true)
    );
    // A physical lease of this boot, expired, with nothing still running: the sweep marks it
    // uncertain, verifies quiescence and transfers it.
    let boot_id = store.info().boot_id.unwrap();
    let resource = desk.id.clone();
    write(&*store, move |tx| {
        let fence = tx.resources().next_fence(&resource)?;
        tx.resources().insert_lease(&StoredLease {
            id: id("lease-1"),
            resource_id: resource,
            phase: LeasePhase::Held,
            end_reason: None,
            owner: ActorId(id("host")),
            holder_key: "actor:host".into(),
            session_id: None,
            project_id: None,
            root_id: None,
            assignment_id: None,
            boot_id,
            purpose: None,
            priority: 1,
            units: 1,
            lease_ms: 1_000,
            queue_seq: 1,
            fence,
            requested_at_ms: T0,
            granted_at_ms: Some(T0),
            renewed_at_ms: None,
            expires_at_ms: T0 + 1_000,
            ended_at_ms: None,
            unreachable_since_ms: None,
            revision: 1,
        })
    })
    .unwrap();
    h.advance(2_000);
    let (cause, report) = rt.block_on(keeper.tick()).unwrap();
    assert_eq!(cause, ReconcileCause::Sweep);
    assert_eq!((report.uncertain, report.reconciled), (1, 1));
    let lease = read(&*store, |r| r.resources().lease(&id("lease-1"))).unwrap();
    assert_eq!(lease.end_reason, Some(LeaseEndReason::Reconciled));
    // The wall clock jumps far beyond the monotonic clock: a sleep/wake, recorded.
    h.advance(3_600_000);
    let (cause, _) = rt.block_on(keeper.tick()).unwrap();
    assert_eq!(cause, ReconcileCause::Wake);
    let causes: Vec<String> = events(&*store, "ResourcesReconciled")
        .iter()
        .map(|e| e.payload["cause"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(causes, ["Boot", "Sweep", "Wake"]);
    let stopped = rt
        .block_on(keeper.shutdown(Duration::from_millis(100), Duration::from_secs(1)))
        .into_result()
        .unwrap();
    assert!(stopped.is_empty());
    assert_eq!(
        events(&*store, "ResourcesReconciled")
            .last()
            .unwrap()
            .payload["cause"],
        "Shutdown"
    );
}
