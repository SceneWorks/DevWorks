//! The one write fence of the services that run blocking writes beside shutdown.
//!
//! A unit of work is admitted under the fence's read side and holds it through its writer's
//! commit acknowledgment. Closing the fence refuses every later unit and returns only after
//! each admitted unit has finished, so no write outlives the close. The stop flag is an atomic
//! (not state behind the lock), so asking whether the fence is closed never waits, even inside
//! a unit that holds the read side while a close is queued.
use crate::ports::storage::{
    ApplicationStore, StorageError, WriteOptions, WriteTransaction, run_write,
};
use crate::protocol::Id;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

#[derive(Default)]
pub struct WriteFence {
    stopping: AtomicBool,
    units: RwLock<()>,
    admitted: Mutex<HashSet<Id>>,
}
impl WriteFence {
    /// Refuses units not yet admitted, without waiting for admitted ones.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }
    /// [`Self::stop`], then waits for every admitted unit's commit acknowledgment.
    pub fn close(&self) {
        self.stop();
        drop(self.units.write().unwrap_or_else(|p| p.into_inner()));
    }
    /// Whether the fence refuses new units. Volatile host previews must disappear when
    /// shutdown fences the owned producer, even when an unclosed transport still holds its
    /// sink alive.
    pub fn is_closed(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
    /// Attempts registered by [`Self::admit`] and not yet forgotten. Registered before the
    /// admission unit's fence is released, including the gap before a producer installs its
    /// cancellation token. Shutdown never scans settled history.
    pub fn admitted(&self) -> HashSet<Id> {
        self.admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn forget_closed(&self, id: &Id) {
        self.admitted
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
    /// Trusted host publication preparation may stage bounded local artifacts under this fence.
    /// Never call a provider, network transport or package process inside this closure.
    pub fn publication<T>(
        &self,
        work: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let admitted = self.units.read().unwrap_or_else(|p| p.into_inner());
        if self.is_closed() {
            return Err(StorageError::Cancelled);
        }
        let result = work();
        drop(admitted);
        result
    }
    /// One database unit, refused once the fence is closed. No read, parser, provider call or
    /// external fetch runs inside it.
    pub fn write<T, F>(&self, store: &dyn ApplicationStore, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        self.admit(store, work, |_| None)
    }
    /// [`Self::write`] that also registers the attempt `attempt` names in the committed value,
    /// before the unit's fence is released.
    pub fn admit<T, F>(
        &self,
        store: &dyn ApplicationStore,
        work: F,
        attempt: impl FnOnce(&T) -> Option<Id>,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let unit = self.units.read().unwrap_or_else(|p| p.into_inner());
        if self.is_closed() {
            return Err(StorageError::Cancelled);
        }
        let result = run_write(store, WriteOptions::observed(), work);
        if let Some(id) = result.as_ref().ok().and_then(attempt) {
            self.admitted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id);
        }
        drop(unit);
        result
    }
}
