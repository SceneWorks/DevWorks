//! Receipt reply publication through the durable delivery dispatcher on the real SQLite store
//! and real managed files: commands through the gateway, their receipts' replies published by
//! the dispatcher's asynchronous consumer after commit, recovery after a restart, a transient
//! fault outlasting any attempt budget, and a per-consumer in-flight cap.
mod common;

use common::*;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::files::{
    ArtifactFiles, ArtifactSources, FileFault, PublishedBlob, ResolvedSource, SourceRefusal,
};
use devworks_core::ports::storage::ApplicationStore;
use devworks_core::protocol::data::ArtifactSource;
use devworks_core::protocol::records::{ReceiptLookupState, ReceiptObservation};
use devworks_core::protocol::results::{DomainCommandReply, DomainQueryReply, OperationResult};
use devworks_core::protocol::{Check, CommandReply, ProjectId};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::replies::{REPLY_CONSUMER, ReceiptReplies};
use devworks_core::services::history::HistoryService;
use devworks_core::services::sessions::SessionService;
use devworks_engine::replies::reply_consumer;
use devworks_engine::{AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome};
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The native adapter, with the next `fail` publications failing on a full volume.
struct Flaky {
    inner: NativeArtifactFiles,
    fail: AtomicUsize,
}
impl ArtifactFiles for Flaky {
    fn publish(
        &self,
        s: &str,
        src: &mut dyn Read,
        d: &str,
        n: u64,
        r: Option<&str>,
        g: &str,
    ) -> Result<PublishedBlob, FileFault> {
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(FileFault::DiskFull);
        }
        self.inner.publish(s, src, d, n, r, g)
    }
    fn managed_path(&self, d: &str, g: &str) -> Result<String, FileFault> {
        self.inner.managed_path(d, g)
    }
    fn present(&self, d: &str, g: &str, n: u64) -> Result<bool, FileFault> {
        self.inner.present(d, g, n)
    }
    fn inspect(&self, d: &str, g: &str) -> Result<Option<u64>, FileFault> {
        self.inner.inspect(d, g)
    }
    fn read_range(&self, d: &str, g: &str, n: u64, o: u64, m: usize) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range(d, g, n, o, m)
    }
    fn open(&self, d: &str, g: &str, n: u64) -> Result<Box<dyn Read + Send>, FileFault> {
        self.inner.open(d, g, n)
    }
    fn unlink(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.unlink(d, g)
    }
    fn quarantine(&self, d: &str, g: &str) -> Result<(), FileFault> {
        self.inner.quarantine(d, g)
    }
    fn staged(&self) -> Result<Vec<String>, FileFault> {
        self.inner.staged()
    }
    fn remove_staged(&self, n: &str) -> Result<(), FileFault> {
        self.inner.remove_staged(n)
    }
    fn stored_files(&self) -> Result<Vec<(String, String)>, FileFault> {
        self.inner.stored_files()
    }
    fn write_derived(&self, k: &str, b: &[u8]) -> Result<(), FileFault> {
        self.inner.write_derived(k, b)
    }
    fn read_derived(&self, k: &str, m: usize) -> Result<Option<Vec<u8>>, FileFault> {
        self.inner.read_derived(k, m)
    }
    fn collect_derived(&self) -> Result<u64, FileFault> {
        self.inner.collect_derived()
    }
    fn export(&self, d: &str, g: &str, n: u64, dest: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, dest, o)
    }
}

struct NoSources;
impl ArtifactSources for NoSources {
    fn resolve(
        &self,
        _: &InvocationContext,
        _: &ArtifactSource,
        _: Option<&ProjectId>,
    ) -> Result<ResolvedSource, SourceRefusal> {
        Err(SourceRefusal::NotFound)
    }
}

struct Boot {
    store: Arc<dyn ApplicationStore>,
    cases: UseCases,
    replies: Arc<ReceiptReplies>,
}
fn boot(h: &Harness) -> Boot {
    boot_failing(h, 0)
}
/// A boot whose first `failures` reply file publications fail on a full volume.
fn boot_failing(h: &Harness, failures: usize) -> Boot {
    let store = h.open();
    let files = Arc::new(Flaky {
        inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
        fail: AtomicUsize::new(failures),
    });
    let svc = Arc::new(ArtifactService::new(
        files,
        Arc::new(NoSources),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let cases = UseCases::new()
        .with_artifacts(svc.clone())
        .with_sessions(Arc::new(SessionService::new(h.ids.clone(), store.clone())))
        .with_history(Arc::new(HistoryService::new(h.ids.clone())));
    Boot {
        store,
        cases,
        replies: Arc::new(ReceiptReplies::new(svc)),
    }
}
impl Boot {
    fn start(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> DomainCommandReply {
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*h.ids,
        };
        let frame = json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key,
            "body": {"kind": "sessions.start", "scope": {"kind": "Workspace"}, "title": "Home"}});
        self.cases
            .command(
                &gateway,
                ctx,
                "sessions.start",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    fn receipt(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> DomainQueryReply {
        let gateway = Gateway {
            authority: &Authority,
            store: &*self.store,
            ids: &*h.ids,
        };
        let frame = json!({"protocol": "1.0-draft.1", "queryId": "q-r",
            "query": {"kind": "history.command_receipt", "requestKey": key}});
        self.cases
            .query(
                &gateway,
                ctx,
                "history.command_receipt",
                frame.to_string().as_bytes(),
            )
            .expect("addressable")
    }
    fn observation(&self, h: &Harness, ctx: &InvocationContext, key: &str) -> ReceiptObservation {
        match self.receipt(h, ctx, key) {
            DomainQueryReply::Result {
                result: OperationResult::HistoryCommandReceipt { value },
                ..
            } => value,
            other => panic!("{other:?}"),
        }
    }
    fn dispatcher(&self, h: &Harness) -> devworks_engine::Dispatcher {
        dispatcher(
            h,
            self.store.clone(),
            vec![reply_consumer(self.replies.clone(), self.store.clone())],
        )
    }
}

#[test]
fn the_dispatcher_publishes_every_recorded_receipt_reply() {
    let h = Harness::new();
    let rt = runtime();
    let b = boot(&h);
    let user = principal(&*b.store, "user-a", PrincipalKind::Human);
    let first = b.start(&h, &user, "start-1");
    let second = b.start(&h, &user, "start-2");
    assert!(matches!(first, CommandReply::Recorded { .. }));
    assert!(matches!(
        b.receipt(&h, &user, "start-1"),
        DomainQueryReply::Rejected { .. }
    ));
    let d = b.dispatcher(&h);
    let claimed = rt.block_on(async {
        within("publication", async {
            d.recover().await.unwrap();
            d.run_until_idle().await.unwrap()
        })
        .await
    });
    assert_eq!(claimed, 2);
    let status = read(&*b.store, |s| s.queue().status(REPLY_CONSUMER));
    assert_eq!((status.completed, status.ready, status.failed), (2, 0, 0));
    for (key, reply) in [("start-1", &first), ("start-2", &second)] {
        let o = b.observation(&h, &user, key);
        o.check().unwrap();
        assert!(matches!(o.state, ReceiptLookupState::Recorded));
        let CommandReply::Recorded { receipt, .. } = reply else {
            panic!()
        };
        assert_eq!(o.command_id.as_ref(), Some(&receipt.command_id));
        assert!(o.reply_artifact.is_some() && o.reply_schema.is_some());
    }
    // Nothing is left to deliver, and a further pass publishes nothing again.
    let again = rt.block_on(async { d.run_until_idle().await.unwrap() });
    assert_eq!(again, 0);
}

#[test]
fn an_unpublished_reply_is_published_by_the_next_boot() {
    let h = Harness::new();
    let rt = runtime();
    let user;
    {
        let b = boot(&h);
        user = principal(&*b.store, "user-a", PrincipalKind::Human);
        assert!(matches!(
            b.start(&h, &user, "start-1"),
            CommandReply::Recorded { .. }
        ));
        // The process stops before any dispatcher ran.
    }
    let b = boot(&h);
    let user = principal(&*b.store, user.principal().0.as_str(), PrincipalKind::Human);
    let d = b.dispatcher(&h);
    let claimed = rt.block_on(async {
        within("recovery", async {
            d.recover().await.unwrap();
            d.run_until_idle().await.unwrap()
        })
        .await
    });
    assert_eq!(claimed, 1);
    b.observation(&h, &user, "start-1").check().unwrap();
}

#[test]
fn a_transient_fault_outlasting_any_attempt_budget_still_publishes() {
    let h = Harness::new();
    let rt = runtime();
    // A full volume for longer than the default retry budget (8 attempts) and its window.
    let b = boot_failing(&h, 12);
    let user = principal(&*b.store, "user-a", PrincipalKind::Human);
    assert!(matches!(
        b.start(&h, &user, "start-1"),
        CommandReply::Recorded { .. }
    ));
    let d = b.dispatcher(&h);
    let mut attempts = 0;
    let published = rt.block_on(async {
        within("publication through a long fault", async {
            d.recover().await.unwrap();
            for _ in 0..40 {
                attempts += d.run_until_idle().await.unwrap();
                let status = read(&*b.store, |s| s.queue().status(REPLY_CONSUMER));
                assert_eq!(status.failed, 0, "never terminal");
                if status.completed == 1 {
                    return true;
                }
                // Past the capped backoff (5 minutes).
                h.advance(301_000);
            }
            false
        })
        .await
    });
    assert!(published, "published after {attempts} attempts");
    assert_eq!(attempts, 13);
    b.observation(&h, &user, "start-1").check().unwrap();
}

/// An asynchronous handler that waits until its gate opens.
struct Gated {
    gate: tokio::sync::watch::Receiver<bool>,
    started: Arc<AtomicUsize>,
}
impl AsyncDeliveryHandler for Gated {
    fn start(&self, _: AsyncDelivery) -> AsyncFuture {
        self.started.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        Box::pin(async move {
            while !*gate.borrow() {
                if gate.changed().await.is_err() {
                    break;
                }
            }
            AsyncOutcome::Committed
        })
    }
}

#[test]
fn a_capped_consumer_burst_leaves_capacity_to_the_others() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    for _ in 0..3 {
        enqueue(&*store, "burst", None, json!({}));
    }
    enqueue(&*store, "other", None, json!({}));
    let (open, gate) = tokio::sync::watch::channel(false);
    let started = Arc::new(AtomicUsize::new(0));
    let mut burst = consumer("burst", Recorder::ok());
    burst.async_handler = Some(Arc::new(Gated {
        gate,
        started: started.clone(),
    }));
    burst.max_in_flight = Some(1);
    let other = Recorder::ok();
    let d = dispatcher_with(
        &h,
        store.clone(),
        vec![burst, consumer("other", other.clone())],
        |c| c.max_in_flight = 3,
    );
    rt.block_on(async {
        within("fair dispatch", async {
            d.recover().await.unwrap();
            let mut changes = d.subscribe();
            // Three free permits: the burst may take one, the other consumer gets its own.
            assert_eq!(d.tick().await.unwrap().claimed, 2);
            while other.seen().is_empty() {
                changes.changed().await.unwrap();
            }
            // While the burst's delivery runs, it claims no more, whatever capacity is free.
            assert_eq!(d.tick().await.unwrap().claimed, 0);
            assert_eq!(read(&*store, |s| s.queue().status("burst")).claimed, 1);
            open.send(true).unwrap();
            d.run_until_idle().await.unwrap();
        })
        .await
    });
    assert_eq!(read(&*store, |s| s.queue().status("burst")).completed, 3);
    assert_eq!(started.load(Ordering::SeqCst), 3);
}

/// An asynchronous handler whose first delivery panics; later ones commit.
struct PanicsOnce(AtomicUsize);
impl AsyncDeliveryHandler for PanicsOnce {
    fn start(&self, _: AsyncDelivery) -> AsyncFuture {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                panic!("handler fault");
            }
            AsyncOutcome::Committed
        })
    }
}

#[test]
fn a_panicking_handler_releases_its_slot_and_the_consumer_keeps_going() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    for _ in 0..2 {
        enqueue(&*store, "capped", None, json!({}));
    }
    let handler = Arc::new(PanicsOnce(AtomicUsize::new(0)));
    let mut capped = consumer("capped", Recorder::ok());
    capped.async_handler = Some(handler.clone());
    capped.max_in_flight = Some(1);
    let d = dispatcher(&h, store.clone(), vec![capped]);
    rt.block_on(async {
        within("dispatch after a panic", async {
            d.recover().await.unwrap();
            // The first delivery's handler panics; its slot is released, so the capped consumer
            // still delivers the second, and the pass terminates.
            assert_eq!(d.run_until_idle().await.unwrap(), 2);
            assert_eq!(d.in_flight(), 0);
            // The panicked claim expires into ordinary recovery and is delivered again.
            h.advance(31_000);
            assert_eq!(d.run_until_idle().await.unwrap(), 1);
        })
        .await
    });
    assert_eq!(read(&*store, |s| s.queue().status("capped")).completed, 2);
    assert_eq!(handler.0.load(Ordering::SeqCst), 3);
}

#[test]
fn concurrent_passes_respect_a_consumer_cap_together() {
    let h = Harness::new();
    let rt = runtime();
    let store = h.open();
    for _ in 0..3 {
        enqueue(&*store, "burst", None, json!({}));
    }
    let (open, gate) = tokio::sync::watch::channel(false);
    let mut burst = consumer("burst", Recorder::ok());
    burst.async_handler = Some(Arc::new(Gated {
        gate,
        started: Arc::new(AtomicUsize::new(0)),
    }));
    burst.max_in_flight = Some(1);
    let d = dispatcher_with(&h, store.clone(), vec![burst], |c| {
        c.max_in_flight = 3;
        c.claim_batch = 1;
    });
    rt.block_on(async {
        within("concurrent passes", async {
            d.recover().await.unwrap();
            // Two passes interleave at their claim writes.
            let (d1, d2) = (d.clone(), d.clone());
            let a = tokio::spawn(async move { d1.tick().await });
            let b = tokio::spawn(async move { d2.tick().await });
            let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
            assert_eq!(a.claimed + b.claimed, 1);
            assert_eq!(read(&*store, |s| s.queue().status("burst")).claimed, 1);
            open.send(true).unwrap();
            d.run_until_idle().await.unwrap();
        })
        .await
    });
    assert_eq!(read(&*store, |s| s.queue().status("burst")).completed, 3);
}
