//! Durable approval readmission. The core restores an opaque original invocation from its
//! immutable reviewed outbox; this driver only submits it to the ordinary gateway and settles
//! the recorded outcome. Approval never changes an Agent into a Human or clears a run hold.
use crate::store;
use crate::workers::Ticker;
use devworks_core::context::InvocationContext;
use devworks_core::gateway::CommandOutcome;
use devworks_core::ports::storage::{ApplicationStore, Cancellation, StorageError, WriteOptions};
use devworks_core::protocol::Id;
use devworks_core::services::approvals::{ApprovalService, ResumePreparation};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::watch;

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
    ticker: Ticker,
    cancellation: Cancellation,
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
        Self::build(store, service, admission, OnceLock::new())
    }
    /// [`Self::new`] with what runs after each settled readmission.
    pub fn with_settled(
        store: Arc<dyn ApplicationStore>,
        service: Arc<ApprovalService>,
        admission: Arc<dyn ResumeAdmission>,
        settled: Settled,
    ) -> Arc<Self> {
        Self::build(store, service, admission, OnceLock::from(settled))
    }
    fn build(
        store: Arc<dyn ApplicationStore>,
        service: Arc<ApprovalService>,
        admission: Arc<dyn ResumeAdmission>,
        settled: OnceLock<Settled>,
    ) -> Arc<Self> {
        let (status, _) = watch::channel(None);
        Arc::new(Self {
            store,
            service,
            admission,
            ticker: Ticker::new(),
            cancellation: Cancellation::new(),
            ticking: tokio::sync::Mutex::new(()),
            status,
            settled,
        })
    }
    /// Sets what runs after each settled readmission (at most once).
    pub fn on_settled(&self, settled: Settled) {
        let _ = self.settled.set(settled);
    }
    pub fn notify(&self) {
        self.ticker.notify();
    }
    pub fn watch(&self) -> watch::Receiver<Option<Result<ResumePass, StorageError>>> {
        self.status.subscribe()
    }
    pub fn stop(&self) {
        self.ticker.stop();
        self.cancellation.cancel();
    }
    /// A blocking gateway call cannot be stopped by aborting its Tokio wrapper. Drain the
    /// preparation/admission/settlement fence so none of its writes outlive runtime shutdown.
    pub async fn drain(&self) {
        self.stop();
        self.ticker.drain().await;
    }
    pub(crate) fn close(&self) {
        self.stop();
        self.ticker.close();
    }
    pub async fn run(self: Arc<Self>, every: Duration) {
        self.ticker
            .run(every, || async {
                let result = self.tick().await;
                self.status.send_replace(Some(result));
            })
            .await;
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
            if self.ticker.is_stopped() {
                return Ok(report);
            }
            let (store, service, fence, stopping, cursor) = (
                self.store.clone(),
                self.service.clone(),
                self.ticker.fence(),
                self.ticker.flag(),
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
            if self.ticker.is_stopped() {
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
                if self.ticker.is_stopped() {
                    break;
                }
                let (store, service, admission, stopping, cancellation, fence, settled) = (
                    self.store.clone(),
                    self.service.clone(),
                    self.admission.clone(),
                    self.ticker.flag(),
                    self.cancellation.clone(),
                    self.ticker.fence(),
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
