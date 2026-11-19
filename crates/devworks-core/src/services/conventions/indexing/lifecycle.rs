//! Count local phases through their writer acknowledgements, without holding a lock while
//! waiting for a writer that may consult the same lifecycle state. Network never holds a phase.
use crate::locks::LockExt;
use crate::ports::storage::{
    ApplicationStore, StorageError, WriteOptions, WriteTransaction, run_write,
};
use std::sync::{Arc, Condvar, Mutex};
#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
}
#[derive(Default)]
pub struct PublicationFence {
    state: Mutex<State>,
    changed: Condvar,
}
pub struct LocalPhase {
    fence: Arc<PublicationFence>,
}
impl PublicationFence {
    pub fn enter(self: &Arc<Self>) -> Result<LocalPhase, StorageError> {
        let mut state = self.state.locked();
        if state.closed {
            return Err(StorageError::Cancelled);
        }
        state.active += 1;
        Ok(LocalPhase {
            fence: self.clone(),
        })
    }
    pub fn is_closed(&self) -> bool {
        self.state.locked().closed
    }
    pub fn begin_close(&self) {
        self.state.locked().closed = true;
    }
    pub fn drain(&self) {
        let mut state = self.state.locked();
        while state.active != 0 {
            state = self.changed.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }
}
impl PublicationFence {
    /// The one guarded write of every worker that publishes locally: a phase is entered (so a
    /// close waits for this write's acknowledgment), and inside the writer the work is refused
    /// once the fence has closed or the options' cancellation was requested.
    pub fn write<T, F>(
        self: &Arc<Self>,
        store: &dyn ApplicationStore,
        options: WriteOptions,
        work: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let _phase = self.enter()?;
        let (closed, cancellation) = (self.clone(), options.cancellation.clone());
        run_write(store, options, move |tx| {
            if closed.is_closed() || cancellation.is_some_and(|c| c.is_cancelled()) {
                return Err(StorageError::Cancelled);
            }
            work(tx)
        })
    }
}
impl Drop for LocalPhase {
    fn drop(&mut self) {
        let mut state = self.fence.state.locked();
        state.active -= 1;
        self.fence.changed.notify_all();
    }
}
