//! Required-input OS delivery over the canonical persistent inbox. OS outcomes never read,
//! acknowledge, decide, grant or resume work. The durable ledger is keyed by user and item,
//! not read/ack revision; recovered or unconfirmed submissions are never blindly repeated.
use crate::dispatch::{Either, first};
use crate::store;
use devworks_core::ports::os_notifications::{
    OsDeliveryOutcome, OsNotification, OsNotificationPermission, OsNotificationPort,
};
use devworks_core::ports::storage::{
    ApplicationStore, ConfigScope, NewEvent, NotificationFilter, NotificationPosition, ReadOptions,
    StorageError, StoredNotification, StoredOsNotificationDelivery, Unavailable, WriteOptions,
    read_snapshot, run_write,
};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::EventId;
use devworks_core::services::notifications::{os_eligible, os_eligible_tx};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
const PAGE: u16 = 64;
const PERMISSION_INTERVAL: Duration = Duration::from_secs(30);
const DEFERRED: &str = "DeferredBeforeSubmission:";

#[derive(Clone, Debug)]
pub struct NotificationPass {
    pub at_ms: i64,
    pub result: Result<usize, StorageError>,
}
struct PermissionState {
    value: OsNotificationPermission,
    checked_at: Option<Instant>,
}

pub struct OsNotificationSupervisor {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    port: Arc<dyn OsNotificationPort>,
    /// 0 unknown, 1 focused, 2 positively unfocused. Unknown fails closed.
    focus: Arc<AtomicU8>,
    stopping: Arc<AtomicBool>,
    fence: Arc<RwLock<bool>>,
    recovered: AtomicBool,
    retry: AtomicBool,
    permission: Mutex<PermissionState>,
    ticking: tokio::sync::Mutex<()>,
    wake: Notify,
    status: watch::Sender<Option<NotificationPass>>,
}
impl OsNotificationSupervisor {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        port: Arc<dyn OsNotificationPort>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        Arc::new(Self {
            store,
            ids,
            clock,
            port,
            focus: Arc::new(AtomicU8::new(0)),
            stopping: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(RwLock::new(false)),
            recovered: AtomicBool::new(false),
            retry: AtomicBool::new(false),
            permission: Mutex::new(PermissionState {
                value: OsNotificationPermission::Unavailable(
                    "Native notification capability has not been checked yet".into(),
                ),
                checked_at: None,
            }),
            ticking: tokio::sync::Mutex::new(()),
            wake: Notify::new(),
            status,
        })
    }
    pub fn notify(&self) {
        self.wake.notify_one();
    }
    pub fn set_focus(&self, focused: Option<bool>) {
        let state = match focused {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        if self.focus.swap(state, Ordering::SeqCst) != state {
            self.notify();
        }
    }
    pub fn permission(&self) -> OsNotificationPermission {
        self.permission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .value
            .clone()
    }
    pub fn watch(&self) -> watch::Receiver<Option<NotificationPass>> {
        self.status.subscribe()
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.notify();
    }
    pub async fn drain(&self) {
        self.stop();
        let fence = self.fence.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *fence.write().unwrap_or_else(|p| p.into_inner()) = true;
        })
        .await;
    }
    pub fn close(&self) {
        self.stop();
        *self.fence.write().unwrap_or_else(|p| p.into_inner()) = true;
    }
    fn cache_permission(&self, value: OsNotificationPermission, explicit: bool) {
        let mut cache = self.permission.lock().unwrap_or_else(|p| p.into_inner());
        // SystemManaged provides no proof that a prior denial was lifted. It can requalify
        // known non-delivery only through the explicit human permission/retry interaction.
        let lifted = matches!(
            cache.value,
            OsNotificationPermission::Denied | OsNotificationPermission::Suppressed(_)
        ) && value == OsNotificationPermission::Allowed;
        if explicit || lifted {
            self.retry.store(true, Ordering::SeqCst);
        }
        cache.value = value;
        cache.checked_at = Some(Instant::now());
    }
    async fn query_permission(&self, request: bool) -> OsNotificationPermission {
        let port = self.port.clone();
        let fence = self.fence.clone();
        let stopping = self.stopping.clone();
        let value = tokio::task::spawn_blocking(move || {
            let closed = fence.read().unwrap_or_else(|p| p.into_inner());
            if *closed || stopping.load(Ordering::SeqCst) {
                return OsNotificationPermission::Unavailable(
                    "Native notification delivery is stopping".into(),
                );
            }
            if request {
                port.request_permission()
            } else {
                port.permission()
            }
        })
        .await
        .unwrap_or_else(|_| {
            OsNotificationPermission::Unavailable(
                "Native notification permission query failed".into(),
            )
        });
        self.cache_permission(value.clone(), request);
        value
    }
    /// Only the host's explicit native-human Settings action calls this, off the GUI thread.
    pub async fn request_permission(&self) -> OsNotificationPermission {
        let _one = self.ticking.lock().await;
        let value = self.query_permission(true).await;
        self.notify();
        value
    }
    pub async fn run(self: Arc<Self>, every: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let result = self.tick().await;
            self.status.send_replace(Some(NotificationPass {
                at_ms: self.clock.now_ms().get(),
                result,
            }));
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            match first(
                self.wake.notified(),
                tokio::time::sleep(every.max(Duration::from_millis(1))),
            )
            .await
            {
                Either::Left(()) | Either::Right(()) => {}
            }
        }
    }
    /// One bounded-page, full keyset sweep. Tests drive this exact production policy directly.
    pub async fn tick(&self) -> Result<usize, StorageError> {
        let _one = self.ticking.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Ok(0);
        }
        if !self.recovered.load(Ordering::SeqCst) {
            let stopping = self.stopping.clone();
            store::write(&self.store, move |tx| {
                if stopping.load(Ordering::SeqCst) {
                    return Ok(0);
                }
                tx.decisions().recover_os_notification_deliveries()
            })
            .await?;
            self.recovered.store(true, Ordering::SeqCst);
        }
        let due = self
            .permission
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .checked_at
            .is_none_or(|at| at.elapsed() >= PERMISSION_INTERVAL);
        if due {
            self.query_permission(false).await;
        }
        if self.focus.load(Ordering::SeqCst) != 2 || !self.permission().can_submit() {
            return Ok(0);
        }
        let retry = self.retry.swap(false, Ordering::SeqCst);
        let mut after: Option<NotificationPosition> = None;
        let mut submitted = 0;
        let mut failure = None;
        loop {
            if self.stopping.load(Ordering::SeqCst) || self.focus.load(Ordering::SeqCst) != 2 {
                break;
            }
            let cursor = after.clone();
            let rows = store::read(&self.store, move |snapshot| {
                let users = snapshot.decisions().notification_users()?;
                let user = match users.as_slice() {
                    [] => return Ok(Vec::new()),
                    [user] => user,
                    _ => {
                        return Err(StorageError::InvalidRecord(
                            "Multiple canonical notification recipients are bound".into(),
                        ));
                    }
                };
                let filter = NotificationFilter {
                    user_id: user.clone(),
                    scope: ConfigScope::Workspace,
                    categories: Vec::new(),
                    read: None,
                    unacknowledged_only: true,
                    completion_only: false,
                };
                let rows = snapshot
                    .decisions()
                    .notifications(&filter, cursor.as_ref(), PAGE)?;
                let mut out = Vec::new();
                for row in rows {
                    let eligible = os_eligible(snapshot, user, &row)?;
                    let delivery = snapshot
                        .decisions()
                        .os_notification_delivery(user, &row.id)?;
                    out.push((row, eligible, delivery));
                }
                Ok(out)
            })
            .await?;
            let full = rows.len() == PAGE as usize;
            for (row, eligible, previous) in rows {
                after = Some(NotificationPosition {
                    created_at_ms: row.created_at_ms,
                    id: row.id.clone(),
                });
                if !eligible || !retryable(previous.as_ref(), retry) {
                    continue;
                }
                match self.deliver(row, previous).await {
                    Ok(true) => submitted += 1,
                    Ok(false) => {}
                    Err(e) => {
                        if failure.is_none() {
                            failure = Some(e);
                        }
                    }
                }
                if self.stopping.load(Ordering::SeqCst) || self.focus.load(Ordering::SeqCst) != 2 {
                    break;
                }
            }
            if !full {
                break;
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(submitted),
        }
    }
    async fn deliver(
        &self,
        row: StoredNotification,
        previous: Option<StoredOsNotificationDelivery>,
    ) -> Result<bool, StorageError> {
        let user = row.user_id.clone();
        let id = row.id.clone();
        let focus = self.focus.clone();
        let stopping = self.stopping.clone();
        let claimed = store::write(&self.store, move |tx| {
            if stopping.load(Ordering::SeqCst) || focus.load(Ordering::SeqCst) != 2 {
                return Ok(None);
            }
            let Some(current) = tx.decisions().notification(&user, &id)? else {
                return Ok(None);
            };
            if !os_eligible_tx(tx, &user, &current)? {
                return Ok(None);
            }
            let claim = tx.decisions().claim_os_notification_delivery(
                &user,
                &id,
                previous.as_ref().map(|p| p.revision),
                previous.is_some(),
            )?;
            Ok(Some((current, claim)))
        })
        .await;
        let (row, claim) = match claimed {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(false),
            Err(
                StorageError::RevisionConflict(_)
                | StorageError::Duplicate { .. }
                | StorageError::LeaseLost { .. },
            ) => return Ok(false),
            Err(e) => return Err(e),
        };
        let store = self.store.clone();
        let port = self.port.clone();
        let focus = self.focus.clone();
        let stopping = self.stopping.clone();
        let fence = self.fence.clone();
        let ids = self.ids.clone();
        tokio::task::spawn_blocking(move || {
            // Keep this one admitted external attempt owned through its durable outcome. Drain
            // fences new effects and waits for this section rather than orphaning a callback.
            let closed = fence.read().unwrap_or_else(|p| p.into_inner());
            if *closed { return Ok(false); }
            let permitted = !*closed && !stopping.load(Ordering::SeqCst) && focus.load(Ordering::SeqCst) == 2;
            let current = if permitted {
                read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
                    let Some(current) = snapshot.decisions().notification(&row.user_id, &row.id)? else { return Ok(false) };
                    os_eligible(snapshot, &row.user_id, &current)
                })?
            } else { false };
            let outcome = if current && focus.load(Ordering::SeqCst) == 2 && !stopping.load(Ordering::SeqCst) {
                port.submit(&OsNotification { notification_id: row.id.clone(), title: row.title.clone(), body: row.body.clone() })
            } else { OsDeliveryOutcome::Suppressed(format!("{DEFERRED} focus, lifecycle or source eligibility changed")) };
            let submitted = matches!(outcome, OsDeliveryOutcome::Submitted { .. });
            let (state, detail) = outcome_record(outcome);
            run_write(&*store, WriteOptions::default(), move |tx| {
                tx.decisions().settle_os_notification_delivery(&row.user_id, &row.id, &claim.fence(), state, detail.as_deref())?;
                let event = EventId(ids.next_id(IdKind::Event));
                let actor = tx.store().host_actor.clone();
                tx.history().append(NewEvent { id: event, actor, project_id: row.project_id, root_id: row.root_id, operation_id: None,
                    aggregate_kind: "os_notification_delivery".into(), aggregate_id: format!("{}:{}", row.user_id.0.as_str(), row.id.as_str()), aggregate_revision: claim.revision + 1,
                    kind: "OsNotificationDeliverySettled".into(), payload: json!({"notificationId":row.id.as_str(), "userId":row.user_id.0.as_str(), "state":state, "detail":detail, "attempt":claim.attempt, "submittedIsVisibilityConfirmed":false}) })?;
                Ok(())
            })?;
            Ok(submitted)
        }).await.unwrap_or_else(|_| Err(StorageError::unavailable(Unavailable::HandlerPanicked, "Native notification delivery task failed")))
    }
}
fn retryable(previous: Option<&StoredOsNotificationDelivery>, explicit: bool) -> bool {
    match previous {
        None => true,
        Some(p)
            if p.state == "Suppressed"
                && p.detail.as_deref().is_some_and(|d| d.starts_with(DEFERRED)) =>
        {
            true
        }
        Some(p) => explicit && matches!(p.state.as_str(), "Denied" | "Suppressed" | "Unavailable"),
    }
}
fn outcome_record(outcome: OsDeliveryOutcome) -> (&'static str, Option<String>) {
    let (state, detail) = match outcome {
        OsDeliveryOutcome::Submitted { native_id } => (
            "Submitted",
            native_id.map(|id| {
                format!("System API accepted request; visibility unconfirmed; native ID {id}")
            }),
        ),
        OsDeliveryOutcome::Denied => ("Denied", None),
        OsDeliveryOutcome::Suppressed(reason) => ("Suppressed", Some(reason)),
        OsDeliveryOutcome::Unavailable(reason) => ("Unavailable", Some(reason)),
        OsDeliveryOutcome::Unknown(reason) => ("Unknown", Some(reason)),
    };
    (state, detail.map(|s| s.chars().take(1000).collect()))
}
