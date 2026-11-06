//! Delivery policy runs against the real SQLite ledger and current approval source. The
//! scripted OS boundary proves lifecycle policy; it does not claim native presentation.
mod common;
use common::*;
use devworks_core::context::PrincipalKind;
use devworks_core::ports::os_notifications::*;
use devworks_core::ports::storage::*;
use devworks_core::protocol::ActorId;
use devworks_engine::notifications::OsNotificationSupervisor;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

fn actor(name: &str) -> ActorId {
    ActorId(id(name))
}
fn audience() -> ActorId {
    actor("local-user")
}
fn seed(store: &dyn ApplicationStore, count: usize, bind: bool) {
    write(store, move |tx| {
        for (name, kind) in [
            ("local-user", PrincipalKind::Human),
            ("historical-human", PrincipalKind::Human),
            ("producer-agent", PrincipalKind::Agent),
        ] {
            tx.actors().register(&actor(name), kind, name)?;
        }
        if bind {
            tx.decisions().bind_notification_user(&audience())?;
        }
        for n in 0..count {
            let approval = id(&format!("approval-{n}"));
            tx.decisions().put_approval(
                &StoredApproval {
                    id: approval.clone(),
                    scope: ConfigScope::Workspace,
                    revision: 1,
                    state: "Pending".into(),
                    operation_kind: "SavedOperation".into(),
                    target_id: id("target"),
                    target_revision: 1,
                    content_digest: "a".repeat(64),
                    scope_digest: "b".repeat(64),
                    summary: "Review exact operation".into(),
                    artifact: None,
                    payload: json!({"kind":"SavedOperation","originalActor":"producer-agent"}),
                    author: actor("producer-agent"),
                    author_kind: PrincipalKind::Agent,
                    created_at_ms: T0,
                    decided_by: None,
                    decided_at_ms: None,
                    rationale: None,
                    restore_epoch: 0,
                },
                None,
            )?;
            tx.decisions().put_notification(
                &StoredNotification {
                    id: id(&format!("notice-{n}")),
                    revision: 1,
                    user_id: audience(),
                    project_id: None,
                    root_id: None,
                    decision_id: Some(approval.clone()),
                    category: "RequiredInput".into(),
                    priority: 1,
                    title: "Human input required".into(),
                    body: "Review the persistent exact proposal".into(),
                    source_id: format!("approval-Generic-{}", approval.as_str()),
                    links: json!([]),
                    requires_input: true,
                    read: false,
                    acknowledged: false,
                    created_at_ms: T0 + n as i64,
                    updated_at_ms: T0 + n as i64,
                },
                None,
            )?;
        }
        Ok(())
    })
    .unwrap();
}
#[derive(Debug)]
struct ScriptedOs {
    permission: Mutex<OsNotificationPermission>,
    outcomes: Mutex<VecDeque<OsDeliveryOutcome>>,
    calls: Mutex<Vec<OsNotification>>,
    gate: Option<Arc<Gate>>,
}
#[derive(Debug, Default)]
struct Gate {
    started: Notify,
    released: Mutex<bool>,
    release: Condvar,
}
impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
impl ScriptedOs {
    fn new(permission: OsNotificationPermission, outcomes: Vec<OsDeliveryOutcome>) -> Arc<Self> {
        Arc::new(Self {
            permission: Mutex::new(permission),
            outcomes: Mutex::new(outcomes.into()),
            calls: Mutex::new(Vec::new()),
            gate: None,
        })
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}
impl OsNotificationPort for ScriptedOs {
    fn permission(&self) -> OsNotificationPermission {
        self.permission.lock().unwrap().clone()
    }
    fn request_permission(&self) -> OsNotificationPermission {
        self.permission()
    }
    fn submit(&self, notification: &OsNotification) -> OsDeliveryOutcome {
        self.calls.lock().unwrap().push(notification.clone());
        if let Some(gate) = &self.gate {
            gate.started.notify_one();
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.release.wait(released).unwrap();
            }
        }
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(OsDeliveryOutcome::Submitted {
                native_id: Some(notification.notification_id.as_str().into()),
            })
    }
}
fn supervisor(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    port: Arc<ScriptedOs>,
) -> Arc<OsNotificationSupervisor> {
    OsNotificationSupervisor::new(store, h.ids.clone(), h.clock(), port)
}
fn ledger(store: &dyn ApplicationStore, n: usize) -> Option<StoredOsNotificationDelivery> {
    read(store, |r| {
        r.decisions()
            .os_notification_delivery(&audience(), &id(&format!("notice-{n}")))
    })
}
fn assert_no_authority_change(store: &dyn ApplicationStore, n: usize) {
    read(store, |r| {
        let approval = id(&format!("approval-{n}"));
        let source = r.decisions().approval(&approval)?.unwrap();
        assert_eq!(source.state, "Pending");
        assert_eq!(source.author, actor("producer-agent"));
        assert_eq!(source.author_kind, PrincipalKind::Agent);
        assert!(
            r.decisions()
                .human_decision(ApprovalSource::Generic, &approval)?
                .is_none()
        );
        assert!(r.decisions().approval_resume(&approval)?.is_none());
        let notice = r
            .decisions()
            .notification(&audience(), &id(&format!("notice-{n}")))?
            .unwrap();
        assert!(!notice.acknowledged);
        Ok(())
    });
}
#[test]
fn focus_permission_and_actual_bound_audience_gate_delivery_without_losing_the_inbox() {
    runtime().block_on(within("focus/permission/audience", async {
        for permission in [
            OsNotificationPermission::Allowed,
            OsNotificationPermission::Denied,
            OsNotificationPermission::NotDetermined,
            OsNotificationPermission::Suppressed("presentation".into()),
            OsNotificationPermission::Unavailable("no daemon".into()),
        ] {
            let h = Harness::new();
            let store = h.open();
            seed(&*store, 1, false);
            let os = ScriptedOs::new(permission.clone(), vec![]);
            let worker = supervisor(&h, store.clone(), os.clone());
            assert_eq!(worker.tick().await.unwrap(), 0); // unknown focus
            worker.set_focus(Some(true));
            assert_eq!(worker.tick().await.unwrap(), 0);
            worker.set_focus(Some(false));
            assert_eq!(worker.tick().await.unwrap(), 0); // no bound recipient
            write(&*store, |tx| {
                tx.decisions().bind_notification_user(&audience())
            })
            .unwrap();
            let submitted = worker.tick().await.unwrap();
            assert_eq!(submitted, usize::from(permission.can_submit()));
            assert_eq!(os.calls(), submitted);
            assert_eq!(ledger(&*store, 0).is_some(), permission.can_submit());
            assert_no_authority_change(&*store, 0);
            if !permission.can_submit() {
                *os.permission.lock().unwrap() = OsNotificationPermission::Allowed;
                worker.request_permission().await;
                assert_eq!(worker.tick().await.unwrap(), 1);
            }
            worker.drain().await;
        }
    }));
}
#[test]
fn read_revision_and_restart_do_not_repeat_an_accepted_submission() {
    runtime().block_on(within("durable dedup", async {
        let h = Harness::new();
        let store = h.open();
        seed(&*store, 1, true);
        let os = ScriptedOs::new(OsNotificationPermission::Allowed, vec![]);
        let worker = supervisor(&h, store.clone(), os.clone());
        worker.set_focus(Some(false));
        assert_eq!(worker.tick().await.unwrap(), 1);
        write(&*store, |tx| {
            let mut n = tx
                .decisions()
                .notification(&audience(), &id("notice-0"))?
                .unwrap();
            n.revision += 1;
            n.read = true;
            tx.decisions().put_notification(&n, Some(1))
        })
        .unwrap();
        assert_eq!(worker.tick().await.unwrap(), 0);
        worker.request_permission().await;
        assert_eq!(worker.tick().await.unwrap(), 0);
        assert_no_authority_change(&*store, 0);
        worker.drain().await;
        drop(worker);
        drop(store);
        let store = h.open();
        let worker = supervisor(&h, store.clone(), os.clone());
        worker.set_focus(Some(false));
        assert_eq!(worker.tick().await.unwrap(), 0);
        assert_eq!(os.calls(), 1);
        assert_eq!(ledger(&*store, 0).unwrap().state, "Submitted");
        assert!(
            read(&*store, |r| r
                .decisions()
                .notification(&audience(), &id("notice-0")))
            .unwrap()
            .read
        );
        worker.drain().await;
    }));
}
#[test]
fn known_non_delivery_requires_explicit_retry_but_unknown_never_resends() {
    runtime().block_on(within("outcomes", async {
        for outcome in [
            OsDeliveryOutcome::Denied,
            OsDeliveryOutcome::Suppressed("quiet".into()),
            OsDeliveryOutcome::Unavailable("daemon gone".into()),
            OsDeliveryOutcome::Unknown("lost reply".into()),
        ] {
            let h = Harness::new();
            let store = h.open();
            seed(&*store, 1, true);
            let unknown = matches!(outcome, OsDeliveryOutcome::Unknown(_));
            let os = ScriptedOs::new(OsNotificationPermission::Allowed, vec![outcome]);
            let worker = supervisor(&h, store.clone(), os.clone());
            worker.set_focus(Some(false));
            assert_eq!(worker.tick().await.unwrap(), 0);
            assert_eq!(os.calls(), 1);
            assert_eq!(worker.tick().await.unwrap(), 0);
            assert_eq!(os.calls(), 1);
            worker.request_permission().await;
            assert_eq!(worker.tick().await.unwrap(), usize::from(!unknown));
            assert_eq!(os.calls(), if unknown { 1 } else { 2 });
            let delivery = ledger(&*store, 0).unwrap();
            assert_eq!(delivery.attempt, if unknown { 1 } else { 2 });
            assert_eq!(
                delivery.state,
                if unknown { "Unknown" } else { "Submitted" }
            );
            assert_no_authority_change(&*store, 0);
            worker.drain().await;
        }
    }));
}
#[test]
fn an_inflight_attempt_from_a_previous_boot_becomes_unknown_not_a_new_alert() {
    runtime().block_on(within("recovery", async {
        let h = Harness::new();
        let store = h.open();
        seed(&*store, 1, true);
        write(&*store, |tx| {
            tx.decisions()
                .claim_os_notification_delivery(&audience(), &id("notice-0"), None, false)
        })
        .unwrap();
        drop(store);
        let store = h.open();
        let os = ScriptedOs::new(OsNotificationPermission::Allowed, vec![]);
        let worker = supervisor(&h, store.clone(), os.clone());
        worker.set_focus(Some(false));
        assert_eq!(worker.tick().await.unwrap(), 0);
        worker.request_permission().await;
        assert_eq!(worker.tick().await.unwrap(), 0);
        assert_eq!(os.calls(), 0);
        assert_eq!(ledger(&*store, 0).unwrap().state, "Unknown");
        assert_no_authority_change(&*store, 0);
        worker.drain().await;
    }));
}

/// Park one admitted claim, then let real source/focus changes win before its transaction.
struct ParkStore {
    inner: Arc<dyn ApplicationStore>,
    armed: AtomicBool,
    job: Mutex<Option<(WriteOptions, Box<dyn WriteJob>)>>,
    parked: Notify,
}
impl ParkStore {
    fn release(&self) {
        let (options, job) = self.job.lock().unwrap().take().unwrap();
        self.inner.submit_write(options, job).unwrap();
    }
}
impl ApplicationStore for ParkStore {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            *self.job.lock().unwrap() = Some((options, job));
            self.parked.notify_one();
            Ok(())
        } else {
            self.inner.submit_write(options, job)
        }
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}
#[test]
fn source_acknowledgment_resolution_and_focus_are_rechecked_inside_the_claim() {
    runtime().block_on(within("claim requalification", async {
        for change in ["ack", "resolve", "focus", "stop"] {
            let h = Harness::new();
            let store = h.open();
            seed(&*store, 1, true);
            let parked = Arc::new(ParkStore {
                inner: store.clone(),
                armed: AtomicBool::new(false),
                job: Mutex::new(None),
                parked: Notify::new(),
            });
            let os = ScriptedOs::new(OsNotificationPermission::Allowed, vec![]);
            let worker = supervisor(&h, parked.clone(), os.clone());
            worker.tick().await.unwrap(); // recovery, unknown focus
            parked.armed.store(true, Ordering::SeqCst);
            worker.set_focus(Some(false));
            let ticking = tokio::spawn({
                let worker = worker.clone();
                async move { worker.tick().await }
            });
            parked.parked.notified().await;
            match change {
                "ack" => write(&*store, |tx| {
                    let mut n = tx
                        .decisions()
                        .notification(&audience(), &id("notice-0"))?
                        .unwrap();
                    n.revision = 2;
                    n.acknowledged = true;
                    tx.decisions().put_notification(&n, Some(1))
                })
                .unwrap(),
                "resolve" => write(&*store, |tx| {
                    let mut a = tx.decisions().approval(&id("approval-0"))?.unwrap();
                    a.revision = 2;
                    a.state = "Rejected".into();
                    a.decided_by = Some(audience());
                    a.decided_at_ms = Some(T0);
                    a.rationale = Some("No".into());
                    tx.decisions().put_approval(&a, Some(1))
                })
                .unwrap(),
                "focus" => worker.set_focus(Some(true)),
                "stop" => worker.stop(),
                _ => unreachable!(),
            }
            parked.release();
            assert_eq!(ticking.await.unwrap().unwrap(), 0);
            assert_eq!(os.calls(), 0);
            assert!(ledger(&*store, 0).is_none());
            worker.drain().await;
        }
    }));
}
#[test]
fn drain_owns_inflight_submission_and_fences_any_later_effect() {
    runtime().block_on(within("drain", async {
        let h = Harness::new();
        let store = h.open();
        seed(&*store, 2, true);
        let gate = Arc::new(Gate::default());
        let os = Arc::new(ScriptedOs {
            permission: Mutex::new(OsNotificationPermission::Allowed),
            outcomes: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
            gate: Some(gate.clone()),
        });
        let worker = supervisor(&h, store.clone(), os.clone());
        worker.set_focus(Some(false));
        let ticking = tokio::spawn({
            let worker = worker.clone();
            async move { worker.tick().await }
        });
        gate.started.notified().await;
        worker.stop();
        let draining = tokio::spawn({
            let worker = worker.clone();
            async move { worker.drain().await }
        });
        tokio::task::yield_now().await;
        assert!(
            !draining.is_finished(),
            "drain must retain the admitted native attempt"
        );
        gate.release();
        assert_eq!(ticking.await.unwrap().unwrap(), 1);
        draining.await.unwrap();
        assert_eq!(worker.tick().await.unwrap(), 0);
        assert_eq!(os.calls(), 1);
        assert_eq!(ledger(&*store, 1).unwrap().state, "Submitted");
        assert!(ledger(&*store, 0).is_none());
        assert_no_authority_change(&*store, 1);
    }));
}
#[test]
fn completed_newer_pages_do_not_starve_older_pending_delivery() {
    runtime().block_on(within("keyset paging", async {
        let h = Harness::new();
        let store = h.open();
        seed(&*store, 66, true);
        write(&*store, |tx| {
            for n in 1..66 {
                let name = id(&format!("notice-{n}"));
                let c = tx.decisions().claim_os_notification_delivery(
                    &audience(),
                    &name,
                    None,
                    false,
                )?;
                tx.decisions().settle_os_notification_delivery(
                    &audience(),
                    &name,
                    &c.fence(),
                    "Submitted",
                    None,
                )?;
            }
            Ok(())
        })
        .unwrap();
        let os = ScriptedOs::new(OsNotificationPermission::Allowed, vec![]);
        let worker = supervisor(&h, store.clone(), os.clone());
        worker.set_focus(Some(false));
        assert_eq!(worker.tick().await.unwrap(), 1);
        assert_eq!(os.calls(), 1);
        assert_eq!(os.calls.lock().unwrap()[0].notification_id, id("notice-0"));
        worker.drain().await;
    }));
}
