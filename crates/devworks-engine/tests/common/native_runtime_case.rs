//! Admission for independent heavy runtime test cases, not their internal tasks/apps.
use std::sync::{Condvar, Mutex};

const CAPACITY: usize = 2;
static ACTIVE: Mutex<usize> = Mutex::new(0);
static CHANGED: Condvar = Condvar::new();

pub(super) struct NativeRuntimeCase;

pub(super) fn acquire() -> NativeRuntimeCase {
    let mut active = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
    while *active >= CAPACITY {
        active = CHANGED
            .wait(active)
            .unwrap_or_else(|error| error.into_inner());
    }
    *active += 1;
    NativeRuntimeCase
}

impl Drop for NativeRuntimeCase {
    fn drop(&mut self) {
        let mut active = ACTIVE.lock().unwrap_or_else(|error| error.into_inner());
        *active -= 1;
        CHANGED.notify_one();
    }
}
