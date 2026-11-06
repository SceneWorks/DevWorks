//! Admission for independent heavy runtime test cases, not their internal tasks/apps.
use std::sync::{Condvar, Mutex};

const CAPACITY: usize = 2;
static ACTIVE: Mutex<usize> = Mutex::new(0);
static CHANGED: Condvar = Condvar::new();

pub(super) struct NativeRuntimeCase {
    permits: usize,
}

pub(super) fn acquire() -> NativeRuntimeCase {
    acquire_permits(1)
}

pub(super) fn acquire_exclusive() -> NativeRuntimeCase {
    acquire_permits(CAPACITY)
}

fn acquire_permits(permits: usize) -> NativeRuntimeCase {
    let mut active = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
    while *active > CAPACITY - permits {
        active = CHANGED
            .wait(active)
            .unwrap_or_else(|error| error.into_inner());
    }
    *active += permits;
    NativeRuntimeCase { permits }
}

impl Drop for NativeRuntimeCase {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
        *active -= self.permits;
        CHANGED.notify_all();
    }
}
