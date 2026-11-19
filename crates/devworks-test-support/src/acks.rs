use devworks_core::locks::LockExt;
use devworks_core::ports::storage::{
    ApplicationStore, ReadOptions, ReadSnapshot, StorageError, StoreInfo, WriteJob, WriteOptions,
    WriteTransaction,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Runs the decorated job inside the transaction, so a probe can look before and after it.
pub type Run<'a> = &'a mut dyn FnMut(&mut dyn WriteTransaction) -> Result<(), StorageError>;
type Probe =
    Arc<dyn Fn(&mut dyn WriteTransaction, Run<'_>) -> Result<bool, StorageError> + Send + Sync>;

enum Plan {
    Idle,
    /// Let `skip` writes through, then lose the next one: it commits (`commits`) or is rolled
    /// back, and the caller is told `CommitUnknown` either way.
    Next {
        skip: usize,
        commits: bool,
        then_fail_read: bool,
    },
    /// Lose the acknowledgment of the `n`-th confirmed commit from now (1: the next).
    NthCommit(usize),
    /// Lose the acknowledgment of the first write the probe reports a hit for.
    When(Probe),
}

/// One acknowledgment-loss wrapper for any `ApplicationStore`: the write commits (or not, as
/// chosen) but its caller is told `CommitUnknown`, as after a crash between commit and reply.
/// Choose when with [`lose_next`](Self::lose_next), [`lose_nth_commit`](Self::lose_nth_commit) or
/// [`lose_when`](Self::lose_when); a plan fires once and then the store is transparent again.
/// [`fail_next_read`](Self::fail_next_read) makes the next read fail instead.
pub struct LoseAcks {
    inner: Arc<dyn ApplicationStore>,
    plan: Arc<Mutex<Plan>>,
    read_failure: Mutex<Option<StorageError>>,
    lost: Arc<AtomicUsize>,
}

impl LoseAcks {
    pub fn over(inner: Arc<dyn ApplicationStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            plan: Arc::new(Mutex::new(Plan::Idle)),
            read_failure: Mutex::new(None),
            lost: Arc::new(AtomicUsize::new(0)),
        })
    }
    /// Lets `writes_before` writes through and loses the acknowledgment of the next one. With
    /// `commits` it commits; without, it is rolled back unrun. `then_fail_read` makes the read
    /// after it fail with `Busy`.
    pub fn lose_next(&self, writes_before: usize, commits: bool, then_fail_read: bool) {
        *self.plan.locked() = Plan::Next {
            skip: writes_before,
            commits,
            then_fail_read,
        };
    }
    /// Loses the acknowledgment of the `n`-th confirmed commit from now; the ones before it
    /// are acknowledged normally.
    pub fn lose_nth_commit(&self, n: usize) {
        *self.plan.locked() = Plan::NthCommit(n);
    }
    /// Loses the acknowledgment of the first committed write for which `hit` answers true. `hit`
    /// runs after the job, inside its transaction, and sees what the job wrote.
    pub fn lose_when(
        &self,
        hit: impl Fn(&mut dyn WriteTransaction) -> Result<bool, StorageError> + Send + Sync + 'static,
    ) {
        self.lose_when_around(move |tx, run| {
            run(tx)?;
            hit(tx)
        });
    }
    /// Like [`lose_when`](Self::lose_when), with `probe` running the job (through `run`) so it can
    /// also read what the transaction held before.
    pub fn lose_when_around(
        &self,
        probe: impl Fn(&mut dyn WriteTransaction, Run<'_>) -> Result<bool, StorageError>
        + Send
        + Sync
        + 'static,
    ) {
        *self.plan.locked() = Plan::When(Arc::new(probe));
    }
    /// Cancels a plan that has not fired.
    pub fn disarm(&self) {
        *self.plan.locked() = Plan::Idle;
    }
    /// Whether a plan is waiting to fire.
    pub fn armed(&self) -> bool {
        !matches!(*self.plan.locked(), Plan::Idle)
    }
    /// Fails the next read with `error`.
    pub fn fail_next_read(&self, error: StorageError) {
        *self.read_failure.locked() = Some(error);
    }
    /// Acknowledgments lost so far.
    pub fn lost(&self) -> usize {
        self.lost.load(Ordering::SeqCst)
    }
}

/// Reports a write that ran as `CommitUnknown` once it committed.
struct Lose {
    inner: Box<dyn WriteJob>,
    lost: Arc<AtomicUsize>,
}
impl WriteJob for Lose {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.inner.run(tx)
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        if outcome.is_ok() {
            self.lost.fetch_add(1, Ordering::SeqCst);
        }
        self.inner
            .finish(outcome.and(Err(StorageError::CommitUnknown)));
    }
}
/// Rolls the write back unrun and reports the outcome as unknown.
struct Unrun(Box<dyn WriteJob>);
impl WriteJob for Unrun {
    fn run(&mut self, _: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        Err(StorageError::Busy)
    }
    fn finish(self: Box<Self>, _: Result<(), StorageError>) {
        self.0.finish(Err(StorageError::CommitUnknown));
    }
}
/// Counts confirmed commits down to the one whose acknowledgment is lost.
struct Counting {
    inner: Box<dyn WriteJob>,
    plan: Arc<Mutex<Plan>>,
    lost: Arc<AtomicUsize>,
}
impl WriteJob for Counting {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        self.inner.run(tx)
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        let mut lose = false;
        if outcome.is_ok() {
            let mut plan = self.plan.locked();
            if let Plan::NthCommit(n) = &mut *plan {
                if *n <= 1 {
                    *plan = Plan::Idle;
                    lose = true;
                } else {
                    *n -= 1;
                }
            }
        }
        if lose {
            self.lost.fetch_add(1, Ordering::SeqCst);
            self.inner.finish(Err(StorageError::CommitUnknown));
        } else {
            self.inner.finish(outcome);
        }
    }
}
/// Lets the probe decide, from inside the transaction, whether this write loses its acknowledgment.
struct Probing {
    inner: Box<dyn WriteJob>,
    probe: Probe,
    plan: Arc<Mutex<Plan>>,
    lost: Arc<AtomicUsize>,
    hit: bool,
}
impl WriteJob for Probing {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        let inner = &mut self.inner;
        let mut ran = Ok(());
        let hit = (self.probe)(tx, &mut |tx| {
            ran = inner.run(tx);
            ran.clone()
        })?;
        ran?;
        if hit {
            let mut plan = self.plan.locked();
            if matches!(*plan, Plan::When(_)) {
                *plan = Plan::Idle;
                self.hit = true;
            }
        }
        Ok(())
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        if self.hit && outcome.is_ok() {
            self.lost.fetch_add(1, Ordering::SeqCst);
            self.inner.finish(Err(StorageError::CommitUnknown));
        } else {
            self.inner.finish(outcome);
        }
    }
}

impl ApplicationStore for LoseAcks {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        let (plan, lost) = (self.plan.clone(), self.lost.clone());
        let mut guard = self.plan.locked();
        let wrapped: Box<dyn WriteJob> = match &mut *guard {
            Plan::Idle => job,
            Plan::Next { skip, .. } if *skip > 0 => {
                *skip -= 1;
                job
            }
            Plan::Next {
                commits,
                then_fail_read,
                ..
            } => {
                let (commits, then_fail_read) = (*commits, *then_fail_read);
                *guard = Plan::Idle;
                if then_fail_read {
                    *self.read_failure.locked() = Some(StorageError::Busy);
                }
                if commits {
                    Box::new(Lose { inner: job, lost })
                } else {
                    Box::new(Unrun(job))
                }
            }
            Plan::NthCommit(_) => Box::new(Counting {
                inner: job,
                plan,
                lost,
            }),
            Plan::When(probe) => Box::new(Probing {
                inner: job,
                probe: probe.clone(),
                plan,
                lost,
                hit: false,
            }),
        };
        drop(guard);
        self.inner.submit_write(options, wrapped)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        if let Some(error) = self.read_failure.locked().take() {
            return Err(error);
        }
        self.inner.read(options, work)
    }
}
