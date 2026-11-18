use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{Counter, Id};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// The instant the tests start at (2023-11-14T22:13:20Z).
pub const T0: i64 = 1_700_000_000_000;

/// A wall clock the test moves by hand. The field is public so a harness can share the atomic.
pub struct TestClock(pub AtomicI64);
impl TestClock {
    pub fn new(ms: i64) -> Self {
        Self(AtomicI64::new(ms))
    }
    /// A clock at [`T0`].
    pub fn at_start() -> Self {
        Self::new(T0)
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> Counter {
        Counter::new(self.get()).expect("a positive test time")
    }
}

/// A clock that always reads [`T0`].
pub struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> Counter {
        Counter::new(T0).expect("a positive test time")
    }
}

/// Sequential identifiers: `{kind}-{n}` in lower case, counting from 1 across every kind.
#[derive(Default)]
pub struct TestIds(AtomicU64);
impl TestIds {
    pub fn new() -> Self {
        Self::default()
    }
    /// Identifiers issued so far.
    pub fn issued(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
impl IdSource for TestIds {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).expect("a valid test id")
    }
}
