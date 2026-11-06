//! Count local phases through their writer acknowledgements, without holding a lock while
//! waiting for a writer that may consult the same lifecycle state. Network never holds a phase.
use crate::ports::storage::StorageError;
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
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(StorageError::Cancelled);
        }
        state.active += 1;
        Ok(LocalPhase {
            fence: self.clone(),
        })
    }
    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed
    }
    pub fn begin_close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
    }
    pub fn drain(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.active != 0 {
            state = self.changed.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }
}
impl Drop for LocalPhase {
    fn drop(&mut self) {
        let mut state = self.fence.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active -= 1;
        self.fence.changed.notify_all();
    }
}
