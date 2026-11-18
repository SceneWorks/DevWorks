use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{Counter, Id};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

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
    /// Continues the sequence after `issued` identifiers.
    pub fn set(&self, issued: u64) {
        self.0.store(issued, Ordering::SeqCst);
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

/// A [`TestClock`] that runs a hook on every read, to move time or raise a signal at a chosen
/// read. The hook gets the clock and the number of reads before this one, and runs before the
/// read returns.
pub struct HookedClock {
    inner: Arc<TestClock>,
    reads: AtomicUsize,
    hook: Box<dyn Fn(&TestClock, usize) + Send + Sync>,
}
impl HookedClock {
    pub fn over(
        inner: Arc<TestClock>,
        hook: impl Fn(&TestClock, usize) + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            reads: AtomicUsize::new(0),
            hook: Box::new(hook),
        }
    }
}
impl HookedClock {
    /// Reads of the clock so far.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}
impl Clock for HookedClock {
    fn now_ms(&self) -> Counter {
        (self.hook)(&self.inner, self.reads.fetch_add(1, Ordering::SeqCst));
        self.inner.now_ms()
    }
}

/// Identifiers `{prefix}-{n}` counting from 0 whatever the kind: the correlation ids a gateway
/// test hands the admission path.
pub struct NumberedIds {
    prefix: String,
    next: AtomicU64,
}
impl NumberedIds {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.into(),
            next: AtomicU64::new(0),
        }
    }
}
impl IdSource for NumberedIds {
    fn next_id(&self, _: IdKind) -> Id {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        Id::new(format!("{}-{n}", self.prefix)).expect("a valid test id")
    }
}
