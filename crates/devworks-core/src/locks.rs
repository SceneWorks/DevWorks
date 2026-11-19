//! One policy for a poisoned lock: take the data and carry on.
//!
//! A lock is poisoned when a thread panicked while holding its guard. Every lock in this
//! workspace guards plain data that each critical section leaves consistent at every statement
//! boundary, so recovering is right: the panic is already reported where it happened, and a
//! second panic at the next `lock()` would only spread one failure to unrelated callers.
//!
//! Use [`LockExt::locked`] for a [`Mutex`] and [`RwLockExt::read_locked`] or
//! [`RwLockExt::write_locked`] for an [`RwLock`]. A caller whose guarded data could be left
//! half-updated by a panic should not use these; it should call `lock()` and decide.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Locks a [`Mutex`], recovering the guard if a panicking thread poisoned it.
pub trait LockExt<T: ?Sized> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> LockExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Locks an [`RwLock`] for reading or writing, recovering the guard if it was poisoned.
pub trait RwLockExt<T: ?Sized> {
    fn read_locked(&self) -> RwLockReadGuard<'_, T>;
    fn write_locked(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T: ?Sized> RwLockExt<T> for RwLock<T> {
    fn read_locked(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    fn write_locked(&self) -> RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn poison<T: Send + Sync + 'static>(lock: &Arc<T>, take: impl FnOnce(&T) + Send + 'static) {
        let lock = lock.clone();
        let _ = std::thread::spawn(move || take(&lock)).join();
    }

    #[test]
    fn a_poisoned_mutex_still_yields_its_data() {
        let m = Arc::new(Mutex::new(7));
        poison(&m, |m| {
            let _g = m.lock().unwrap();
            panic!("poison");
        });
        assert!(m.is_poisoned());
        *m.locked() += 1;
        assert_eq!(*m.locked(), 8);
    }

    #[test]
    fn a_poisoned_rwlock_still_yields_its_data_for_read_and_write() {
        let l = Arc::new(RwLock::new(String::from("a")));
        poison(&l, |l| {
            let _g = l.write().unwrap();
            panic!("poison");
        });
        assert!(l.is_poisoned());
        l.write_locked().push('b');
        assert_eq!(*l.read_locked(), "ab");
    }
}
