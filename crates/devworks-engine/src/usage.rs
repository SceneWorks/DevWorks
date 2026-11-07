//! Host-owned inference accounting, bounded historical indexing and local cancellation.
//! This driver executes injected metadata sources, never synthesizes provider inference.
use crate::{dispatch::first, store};
use devworks_core::ports::process::ProcessHost;
use devworks_core::ports::storage::*;
use devworks_core::protocol::Id;
use devworks_core::services::{
    router::InferenceRouter,
    runs::ControlService,
    usage::{
        host::{self, AccountingWriteFence, AttemptHost, DriveOutcome, OwnedInferenceSource},
        ingest,
    },
};
use std::{
    collections::{HashMap, HashSet},
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
    pub recovery_more: bool,
    pub unresolved: usize,
}
#[derive(Clone, Debug)]
pub struct UsageDrain {
    pub unclosed: Vec<Id>,
    pub pending_preparations: usize,
}

struct Recovery {
    after: Option<Id>,
    complete: bool,
    unresolved: usize,
}
pub struct UsageRuntime {
    store: Arc<dyn ApplicationStore>,
    control: Arc<ControlService>,
    host: Option<Arc<AttemptHost>>,
    active: Mutex<HashMap<Id, Cancellation>>,
    uncertain: Mutex<HashSet<Id>>,
    fence: Arc<AccountingWriteFence>,
    process_host: Option<Arc<dyn ProcessHost>>,
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
        Self::build(store, control, router, None)
    }
    pub fn with_process_host(
        store: Arc<dyn ApplicationStore>,
        control: Arc<ControlService>,
        router: Option<Arc<InferenceRouter>>,
        process: Arc<dyn ProcessHost>,
    ) -> Arc<Self> {
        Self::build(store, control, router, Some(process))
    }
    fn build(
        store: Arc<dyn ApplicationStore>,
        control: Arc<ControlService>,
        router: Option<Arc<InferenceRouter>>,
        process_host: Option<Arc<dyn ProcessHost>>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        let fence = Arc::new(AccountingWriteFence::default());
        Arc::new(Self {
            host: router.map(|router| {
                let mut host = AttemptHost::new(store.clone(), control.clone(), router)
                    .with_write_fence(fence.clone());
                if let Some(process) = &process_host {
                    host = host.with_process_host(process.as_ref());
                }
                Arc::new(host)
            }),
            fence,
            process_host,
            uncertain: Mutex::new(HashSet::new()),
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
                unresolved: 0,
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
        struct Remove {
            runtime: Arc<UsageRuntime>,
            id: Id,
        }
        impl Drop for Remove {
            fn drop(&mut self) {
                self.runtime.retired(&self.id);
            }
        }
        let remove = Remove {
            runtime: runtime.clone(),
            id: attempt_id.clone(),
        };
        owned_thread(move || {
            let _remove = remove;
            host.drive(attempt_id, source, cancellation)
        })
        .await
        .unwrap_or_else(|e| Err(e.storage()))
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
        struct Preparing(Arc<UsageRuntime>);
        impl Drop for Preparing {
            fn drop(&mut self) {
                self.0.preparing.fetch_sub(1, Ordering::SeqCst);
                self.0.changed.notify_one();
            }
        }
        let preparing = Preparing(runtime.clone());
        owned_thread(move || {
            let _preparing = preparing;
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
                    self.runtime.retired(&self.id);
                }
            }
            let _remove = Remove {
                runtime: runtime.clone(),
                id: attempt.attempt_id.clone(),
            };
            host.drive(attempt.attempt_id, source, cancellation)
        })
        .await
        .unwrap_or_else(|e| Err(e.storage()))
    }

    fn retired(&self, id: &Id) {
        let closed = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |s| {
            s.usage().attempt_balance(id)
        })
        .ok()
        .flatten()
        .is_some_and(|b| b.local_quiescent);
        if closed {
            self.fence.forget_closed(id);
        } else {
            if let Some(host) = &self.host {
                let _ = host.retain_unclosed(id.clone());
            }
            self.uncertain
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.clone());
        }
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
        self.changed.notify_one();
    }
    async fn write<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let fence = self.fence.clone();
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || fence.write(store.as_ref(), work))
            .await
            .unwrap_or_else(|_| {
                Err(StorageError::unavailable(
                    Unavailable::HandlerPanicked,
                    "accounting unit panicked",
                ))
            })
    }
    /// Bound local-source draining by the runtime's shared deadline, then close the accounting
    /// write fence. Unknown local closure remains unknown; reservations and custody stay held.
    pub async fn drain_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<UsageDrain, StorageError> {
        self.stop();
        let _ = first(tokio::time::sleep_until(deadline), self.drain()).await;
        let fence = self.fence.clone();
        tokio::task::spawn_blocking(move || fence.close())
            .await
            .map_err(|_| {
                StorageError::unavailable(Unavailable::HandlerPanicked, "accounting fence panicked")
            })?;
        let pending_preparations = self.preparing.load(Ordering::SeqCst);
        let mut ids = self.fence.admitted();
        ids.extend(
            self.uncertain
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .cloned(),
        );
        ids.extend(
            self.active
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .keys()
                .cloned(),
        );
        let unclosed = store::read(&self.store, move |s| {
            let mut unclosed = Vec::new();
            for id in ids {
                if !s
                    .usage()
                    .attempt_balance(&id)?
                    .is_some_and(|b| b.local_quiescent)
                {
                    unclosed.push(id);
                }
            }
            unclosed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            Ok(unclosed)
        })
        .await?;
        Ok(UsageDrain {
            unclosed,
            pending_preparations,
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
        let page = self
            .write(move |tx| ingest::bootstrap_page(tx, PAGE))
            .await?;
        let (after, complete) = {
            let r = self.recovery.lock().unwrap_or_else(|p| p.into_inner());
            (r.after.clone(), r.complete)
        };
        let mut recovery_more = false;
        let recovery_complete = if complete {
            true
        } else {
            let boot = self
                .store
                .info()
                .boot_id
                .ok_or_else(|| StorageError::InvalidRecord("missing host boot".into()))?;
            let candidates = store::read(&self.store, move |s| {
                host::recovery_candidates(s, boot, after.as_ref(), PAGE)
            })
            .await?;
            let process = self.process_host.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                host::probe_recovery(candidates, process.as_deref())
            })
            .await
            .unwrap_or_else(|_| {
                Err(StorageError::unavailable(
                    Unavailable::HandlerPanicked,
                    "native owner probe panicked",
                ))
            })?;
            let control = self.control.clone();
            let progress = self
                .write(move |tx| host::apply_recovery(tx, &control, prepared))
                .await?;
            self.uncertain
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(progress.unclosed);
            let mut r = self.recovery.lock().unwrap_or_else(|p| p.into_inner());
            r.unresolved += progress.unresolved;
            recovery_more = progress.visited.len() == usize::from(PAGE);
            if recovery_more {
                r.after = progress.visited.last().cloned();
            } else {
                r.after = None;
                r.complete = r.unresolved == 0;
                r.unresolved = 0;
            }
            r.complete
        };
        Ok(UsagePass {
            indexed: page.visited,
            indexing_complete: page.complete,
            recovery_complete,
            recovery_more,
            unresolved: self
                .uncertain
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            cancelled,
        })
    }
    pub async fn run(self: Arc<Self>, interval: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let result = self.tick().await;
            let more = result
                .as_ref()
                .is_ok_and(|p| !p.indexing_complete || p.recovery_more);
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

// External provider close/start may not implement bounded cancellation. They must not occupy
// Tokio's blocking pool, whose destructor otherwise joins them after the shutdown deadline.
// Registry guards and the publication fence retain exact custody until actual closure.
async fn owned_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, OwnedThreadFailure> {
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("devworks-inference".into())
        .spawn(move || {
            let result = work();
            let _ = send.send(result);
        })
        .map_err(OwnedThreadFailure::Spawn)?;
    receive.await.map_err(|_| OwnedThreadFailure::Lost)
}

enum OwnedThreadFailure {
    Spawn(std::io::Error),
    Lost,
}
impl OwnedThreadFailure {
    fn storage(self) -> StorageError {
        match self {
            Self::Spawn(error) => StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                format!("native inference worker could not start: {error}"),
            ),
            Self::Lost => StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "owned inference worker ended without acknowledgement; accounting remains recoverable",
            ),
        }
    }
}
