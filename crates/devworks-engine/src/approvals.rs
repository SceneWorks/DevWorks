//! Durable approval readmission. The core restores an opaque original invocation from its
//! immutable reviewed outbox; this driver only submits it to the ordinary gateway and settles
//! the recorded outcome. Approval never changes an Agent into a Human or clears a run hold.
use crate::dispatch::first;
use crate::store;
use devworks_core::context::InvocationContext;
use devworks_core::gateway::CommandOutcome;
use devworks_core::ports::storage::{ApplicationStore, Cancellation, StorageError, WriteOptions};
use devworks_core::protocol::Id;
use devworks_core::services::approvals::{ApprovalService, ResumePreparation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use tokio::sync::{Notify, watch};

const PAGE: u16 = 64;

/// The runtime's existing admission path, including command fencing and post-commit wakeups.
/// Implementations must preserve the supplied original invocation and exact saved frame/key.
pub trait ResumeAdmission: Send + Sync {
    fn readmit(
        &self,
        context: &InvocationContext,
        operation: &str,
        frame: &[u8],
        options: WriteOptions,
    ) -> CommandOutcome;
}
#[derive(Clone, Debug, Default)]
pub struct ResumePass {
    pub visited: usize,
    pub waiting: usize,
    pub terminal: usize,
    pub submitted: usize,
}

/// One owned worker, at most one gateway call in flight. ID-keyset sweeps traverse waiting
/// rows as well as ready ones, so paused work at the head cannot starve another approval.
pub struct ResumeDispatcher {
    store: Arc<dyn ApplicationStore>,
    service: Arc<ApprovalService>,
    admission: Arc<dyn ResumeAdmission>,
    stopping: Arc<AtomicBool>,
    cancellation: Cancellation,
    fence: Arc<RwLock<bool>>,
    wake: Notify,
    ticking: tokio::sync::Mutex<()>,
    status: watch::Sender<Option<Result<ResumePass, StorageError>>>,
    settled: OnceLock<Settled>,
}
/// Called with an approval's ID after its readmission outcome is settled.
type Settled = Arc<dyn Fn(&Id) + Send + Sync>;
impl ResumeDispatcher {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        service: Arc<ApprovalService>,
        admission: Arc<dyn ResumeAdmission>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        Arc::new(Self {
            store,
            service,
            admission,
            stopping: Arc::new(AtomicBool::new(false)),
            cancellation: Cancellation::new(),
            fence: Arc::new(RwLock::new(false)),
            wake: Notify::new(),
            ticking: tokio::sync::Mutex::new(()),
            status,
            settled: OnceLock::new(),
        })
    }
    /// Sets what runs after each settled readmission (at most once).
    pub fn on_settled(&self, settled: Settled) {
        let _ = self.settled.set(settled);
    }
    pub fn notify(&self) {
        self.wake.notify_one();
    }
    pub fn watch(&self) -> watch::Receiver<Option<Result<ResumePass, StorageError>>> {
        self.status.subscribe()
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.cancellation.cancel();
        self.notify();
    }
    /// A blocking gateway call cannot be stopped by aborting its Tokio wrapper. Drain the
    /// preparation/admission/settlement fence so none of its writes outlive runtime shutdown.
    pub async fn drain(&self) {
        self.stop();
        let fence = self.fence.clone();
        let _ = tokio::task::spawn_blocking(move || {
            *fence.write().unwrap_or_else(|p| p.into_inner()) = true
        })
        .await;
    }
    pub(crate) fn close(&self) {
        self.stop();
        *self.fence.write().unwrap_or_else(|p| p.into_inner()) = true;
    }
    pub async fn run(self: Arc<Self>, every: Duration) {
        while !self.stopping.load(Ordering::SeqCst) {
            let result = self.tick().await;
            self.status.send_replace(Some(result));
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let _ = first(
                self.wake.notified(),
                tokio::time::sleep(every.max(Duration::from_millis(1))),
            )
            .await;
        }
    }
    /// A complete bounded-memory pass. Storage failures are reported after attempting later
    /// rows; retry uses the same durable outbox identity and gateway request key.
    pub async fn tick(&self) -> Result<ResumePass, StorageError> {
        let _one = self.ticking.lock().await;
        let mut report = ResumePass::default();
        // Required-input projections also catch up after first-user binding and a crash.
        // They share this worker's write fence, so shutdown drains these writes as well.
        let mut notification_after = None;
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(report);
            }
            let (store, service, fence, stopping, cursor) = (
                self.store.clone(),
                self.service.clone(),
                self.fence.clone(),
                self.stopping.clone(),
                notification_after.clone(),
            );
            let next = tokio::task::spawn_blocking(move || {
                let closed = fence.read().unwrap_or_else(|p| p.into_inner());
                if *closed || stopping.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                devworks_core::ports::storage::run_write(
                    &*store,
                    WriteOptions::default(),
                    move |tx| service.publish_pending_notifications(tx, cursor.as_ref(), PAGE),
                )
            })
            .await
            .map_err(|_| {
                StorageError::InvalidRecord("approval notification catch-up task panicked".into())
            })??;
            notification_after = next;
            if notification_after.is_none() {
                break;
            }
        }
        let mut after = None;
        let mut failure = None;
        loop {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let cursor = after.clone();
            let rows = store::read(&self.store, move |s| {
                s.decisions()
                    .pending_approval_resumes(cursor.as_ref(), PAGE)
            })
            .await?;
            let more = rows.len() == usize::from(PAGE);
            after = rows.last().map(|r| r.approval_id.clone());
            for row in rows {
                if self.stopping.load(Ordering::SeqCst) {
                    break;
                }
                let (store, service, admission, stopping, cancellation, fence, settled) = (
                    self.store.clone(),
                    self.service.clone(),
                    self.admission.clone(),
                    self.stopping.clone(),
                    self.cancellation.clone(),
                    self.fence.clone(),
                    self.settled.get().cloned(),
                );
                let result = tokio::task::spawn_blocking(move || {
                    let closed = fence.read().unwrap_or_else(|p| p.into_inner());
                    if *closed || stopping.load(Ordering::SeqCst) {
                        return Ok(Step::Stopped);
                    }
                    match service.prepare_resume(&*store, &row.approval_id)? {
                        ResumePreparation::Terminal => Ok(Step::Terminal),
                        ResumePreparation::Waiting { .. } => Ok(Step::Waiting),
                        ResumePreparation::Ready(command) => {
                            if stopping.load(Ordering::SeqCst) {
                                return Ok(Step::Stopped);
                            }
                            let outcome = admission.readmit(
                                &command.context,
                                &command.operation,
                                &command.frame,
                                WriteOptions {
                                    cancellation: Some(cancellation),
                                    ..WriteOptions::default()
                                },
                            );
                            // Even an unknown outcome is settled under the original binding.
                            // Shutdown waits for this CAS; the next boot retries the same key.
                            service.settle_resume(&*store, &command, &outcome)?;
                            if let Some(settled) = settled {
                                settled(&command.approval_id);
                            }
                            Ok(Step::Submitted)
                        }
                    }
                })
                .await
                .map_err(|_| {
                    StorageError::InvalidRecord("approval readmission task panicked".into())
                })?;
                match result {
                    Ok(Step::Stopped) => break,
                    Ok(step) => {
                        report.visited += 1;
                        match step {
                            Step::Terminal => report.terminal += 1,
                            Step::Waiting => report.waiting += 1,
                            Step::Submitted => report.submitted += 1,
                            Step::Stopped => {}
                        }
                    }
                    Err(e) => {
                        if failure.is_none() {
                            failure = Some(e);
                        }
                    }
                }
            }
            if !more {
                break;
            }
            tokio::task::yield_now().await;
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(report),
        }
    }
}
enum Step {
    Stopped,
    Terminal,
    Waiting,
    Submitted,
}
