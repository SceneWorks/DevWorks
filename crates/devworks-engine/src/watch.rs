//! Watcher supervision (DW-94). Watched processes run on their own process-effect consumer
//! ([`crate::process_tools::watch_consumer`]); this supervisor's pass settles each watcher whose
//! process outcome is observed and delivers its event into the owning session's input. A pass
//! reads only durable state, so a restart between an event's commit and its delivery delivers it
//! on the next pass, once.
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{ApplicationStore, StorageError};
use devworks_core::ports::{IdKind, IdSource};
use devworks_core::protocol::{Counter, Id, Revision};
use devworks_core::services::watch::{WatchService, WatchTick};
use std::sync::Arc;

pub struct WatchSupervisor {
    store: Arc<dyn ApplicationStore>,
    ids: Arc<dyn IdSource>,
    service: Arc<WatchService>,
}

impl WatchSupervisor {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ids: Arc<dyn IdSource>,
        service: Arc<WatchService>,
    ) -> Self {
        Self {
            store,
            ids,
            service,
        }
    }

    pub fn service(&self) -> &Arc<WatchService> {
        &self.service
    }

    fn host(&self) -> InvocationContext {
        let info = self.store.info();
        HostIngress::new(&*self.store).context(
            info.host_actor,
            PrincipalKind::Host,
            self.ids.next_id(IdKind::Invocation),
            Id::new("watch-supervisor").expect("static ingress identity"),
            Counter::new(info.restore_epoch).expect("nonnegative restore epoch"),
            Revision::new(1).expect("positive policy revision"),
        )
    }

    /// One blocking pass: settle observed outcomes, then deliver pending events.
    pub fn tick(&self) -> Result<WatchTick, StorageError> {
        self.service.tick(&self.host())
    }
    /// The settling half of a pass alone: events are recorded, not yet delivered.
    pub fn settle(&self) -> Result<usize, StorageError> {
        self.service.settle_observed()
    }
    /// The last pass of an orderly shutdown, after owned trees were stopped.
    pub fn close_at_shutdown(
        &self,
        stopped: &std::collections::HashSet<Id>,
        deadline: std::time::Instant,
    ) -> Result<usize, StorageError> {
        self.service.close_at_shutdown(stopped, deadline)
    }
    /// The watcher backup check when it is due (its setting, interval and unfinished watchers).
    pub fn backup_if_due(
        &self,
    ) -> Result<Option<devworks_core::services::watch::BackupPass>, StorageError> {
        self.service.backup_if_due(&self.host())
    }
    /// The delivering half of a pass alone.
    pub fn deliver(&self) -> Result<usize, StorageError> {
        self.service.deliver_pending(&self.host())
    }
}
