//! Host-owned inference accounting, bounded historical indexing and local cancellation.
//! This driver executes injected metadata sources, never synthesizes provider inference.
use crate::{dispatch::first, store};
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
use devworks_core::services::{
    router::InferenceRouter,
    runs::ControlService,
    usage::{
        host::{self, AttemptHost, DriveOutcome, OwnedInferenceSource},
        ingest,
    },
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, watch};
const PAGE: u16 = 64;
#[derive(Clone, Debug)]
pub struct UsagePass {
    pub indexed: usize,
    pub indexing_complete: bool,
    pub recovery_complete: bool,
    pub cancelled: usize,
}
struct Recovery {
    after: Option<Id>,
    complete: bool,
}
pub struct UsageRuntime {
    store: Arc<dyn ApplicationStore>,
    control: Arc<ControlService>,
    host: Option<Arc<AttemptHost>>,
    active: Mutex<HashMap<Id, Cancellation>>,
    stopping: AtomicBool,
    preparing: AtomicUsize,
    wake: Notify,
    changed: Notify,
    recovery: Mutex<Recovery>,
    ticking: tokio::sync::Mutex<()>,
    status: watch::Sender<Option<Result<UsagePass, StorageError>>>,
}
impl UsageRuntime {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        control: Arc<ControlService>,
        router: Option<Arc<InferenceRouter>>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        Arc::new(Self {
            host: router
                .map(|router| Arc::new(AttemptHost::new(store.clone(), control.clone(), router))),
            store,
            control,
            active: Mutex::new(HashMap::new()),
            stopping: AtomicBool::new(false),
            preparing: AtomicUsize::new(0),
            wake: Notify::new(),
            changed: Notify::new(),
            recovery: Mutex::new(Recovery {
                after: None,
                complete: false,
            }),
            ticking: tokio::sync::Mutex::new(()),
            status,
        })
    }
    pub fn watch(&self) -> watch::Receiver<Option<Result<UsagePass, StorageError>>> {
        self.status.subscribe()
    }
    pub fn notify(&self) {
        self.wake.notify_one();
    }
    /// The future may be dropped by its caller: ownership stays inside the blocking driver
    /// and the runtime registry until the source has actually closed or visibly failed.
    pub async fn drive(
        self: &Arc<Self>,
        attempt_id: Id,
        source: Box<dyn OwnedInferenceSource>,
    ) -> Result<DriveOutcome, StorageError> {
        let host = self.host.clone().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "no qualified inference router",
            )
        })?;
        let cancellation = Cancellation::new();
        {
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            if self.stopping.load(Ordering::SeqCst) {
                return Err(StorageError::Cancelled);
            }
            match active.entry(attempt_id.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => return Err(StorageError::Busy),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(cancellation.clone());
                }
            }
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            struct Remove {
                runtime: Arc<UsageRuntime>,
                id: Id,
            }
            impl Drop for Remove {
                fn drop(&mut self) {
                    self.runtime
                        .active
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&self.id);
                    self.runtime.changed.notify_one();
                }
            }
            let _remove = Remove {
                runtime: runtime.clone(),
                id: attempt_id.clone(),
            };
            host.drive(attempt_id, source, cancellation)
        })
        .await
        .unwrap_or_else(|_| {
            Err(StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "owned inference source panicked; accounting remains unclosed",
            ))
        })
    }
    /// Full native producer path. Caller cancellation cannot strand an acknowledged
    /// reservation between admission and source ownership: both run in one owned worker.
    pub async fn produce(
        self: &Arc<Self>,
        context: devworks_core::context::InvocationContext,
        logical: devworks_core::services::router::RequestAdmission,
        category: devworks_core::services::router::accounting::ReportCategory,
        request: devworks_core::services::usage::accounting::AttemptRequest,
        source: Box<dyn OwnedInferenceSource>,
    ) -> Result<DriveOutcome, StorageError> {
        let host = self.host.clone().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "no qualified inference router",
            )
        })?;
        {
            let _active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            if self.stopping.load(Ordering::SeqCst) {
                return Err(StorageError::Cancelled);
            }
            self.preparing.fetch_add(1, Ordering::SeqCst);
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || {
            struct Preparing(Arc<UsageRuntime>);
            impl Drop for Preparing {
                fn drop(&mut self) {
                    self.0.preparing.fetch_sub(1, Ordering::SeqCst);
                    self.0.changed.notify_one();
                }
            }
            let _preparing = Preparing(runtime.clone());
            let admission = host.admit(context, logical, category, request)?;
            let attempt = match admission {
                devworks_core::services::usage::accounting::Admission::Admitted {
                    attempt, ..
                } => attempt,
                refusal => return Ok(DriveOutcome::Refused(refusal)),
            };
            let cancellation = Cancellation::new();
            {
                let mut active = runtime.active.lock().unwrap_or_else(|p| p.into_inner());
                match active.entry(attempt.attempt_id.clone()) {
                    std::collections::hash_map::Entry::Occupied(_) => {
                        return Ok(DriveOutcome::AlreadyDispatched);
                    }
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if runtime.stopping.load(Ordering::SeqCst) {
                            cancellation.cancel();
                        }
                        entry.insert(cancellation.clone());
                    }
                }
            }
            struct Remove {
                runtime: Arc<UsageRuntime>,
                id: Id,
            }
            impl Drop for Remove {
                fn drop(&mut self) {
                    self.runtime
                        .active
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&self.id);
                    self.runtime.changed.notify_one();
                }
            }
            let _remove = Remove {
                runtime: runtime.clone(),
                id: attempt.attempt_id.clone(),
            };
            host.drive(attempt.attempt_id, source, cancellation)
        })
        .await
        .unwrap_or_else(|_| {
            Err(StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "owned inference producer panicked; accounting remains recoverable",
            ))
        })
    }

    pub fn stop(&self) {
        let active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        self.stopping.store(true, Ordering::SeqCst);
        for cancellation in active.values() {
            cancellation.cancel();
        }
        self.notify();
    }
    pub async fn drain(&self) {
        self.stop();
        let _tick = self.ticking.lock().await;
        loop {
            let changed = self.changed.notified();
            if self
                .active
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
                && self.preparing.load(Ordering::SeqCst) == 0
            {
                break;
            }
            changed.await;
        }
    }
    pub async fn tick(&self) -> Result<UsagePass, StorageError> {
        let _single = self.ticking.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(StorageError::Cancelled);
        }
        let active = self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect::<Vec<_>>();
        let host = self.host.clone();
        let cancelled = tokio::task::spawn_blocking(move || {
            let mut cancelled = 0;
            for (id, cancellation) in active {
                match host
                    .as_ref()
                    .map(|h| h.cancellation_required(&id))
                    .transpose()
                {
                    Ok(Some(false)) => {}
                    Ok(_) => {
                        cancellation.cancel();
                        cancelled += 1;
                    }
                    Err(error) => {
                        cancellation.cancel();
                        return Err(error);
                    }
                }
            }
            Ok::<_, StorageError>(cancelled)
        })
        .await
        .unwrap_or_else(|_| {
            Err(StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "inference cancellation check panicked",
            ))
        })?;
        let page = store::write(&self.store, move |tx| ingest::bootstrap_page(tx, PAGE)).await?;
        let (after, complete) = {
            let r = self.recovery.lock().unwrap_or_else(|p| p.into_inner());
            (r.after.clone(), r.complete)
        };
        let recovery_complete = if complete {
            true
        } else {
            let control = self.control.clone();
            let visited = store::write(&self.store, move |tx| {
                host::recover_page(tx, &control, after.as_ref(), PAGE)
            })
            .await?;
            let mut r = self.recovery.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(last) = visited.last() {
                r.after = Some(last.clone());
            }
            r.complete = visited.len() < usize::from(PAGE);
            r.complete
        };
        Ok(UsagePass {
            indexed: page.visited,
            indexing_complete: page.complete,
            recovery_complete,
            cancelled,
        })
    }
    pub async fn run(self: Arc<Self>, interval: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let result = self.tick().await;
            let more = result
                .as_ref()
                .is_ok_and(|p| !p.indexing_complete || !p.recovery_complete);
            self.status.send_replace(Some(result));
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            if more {
                tokio::task::yield_now().await;
            } else {
                let _ = first(self.wake.notified(), tokio::time::sleep(interval)).await;
            }
        }
    }
}
