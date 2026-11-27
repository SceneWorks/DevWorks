//! An agent turn's store and file work runs off the runtime thread. On the desktop's
//! current-thread runtime, a turn that waited on the store writer inline would also stop the
//! dispatcher from renewing its leases. Provider bytes are scripted; no model runs.
use super::approval_wait::{Body, pinned_agent_session, runtime_with};
use super::*;
use devworks_core::ports::provider_http::{
    HttpStream, StreamLimits, StreamingHttp, TransportError,
};
use devworks_engine::{AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome};
use std::time::Instant;

/// How long the store writer is held.
const HOLD: Duration = Duration::from_secs(2);

/// The test dispatcher's lease. It renews every third of it, so one renewal falls due inside
/// the hold.
const LEASE_MS: i64 = 3_000;

/// Answers every request with a completed empty answer.
struct DoneWire;
impl StreamingHttp for DoneWire {
    fn open(
        &self,
        _: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        Ok(Box::new(Body(Some(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n\
              data: [DONE]\n\n"
                .to_vec(),
        ))))
    }
}

/// The application's store. Once armed with a session, the first read that finds the session's
/// turn begun holds the writer for [`HOLD`] before it reads, so the turn's first write after
/// `begin_turn` waits on the writer.
struct HoldAfterBegin {
    inner: Arc<dyn ApplicationStore>,
    armed: Mutex<Option<Id>>,
    held: Arc<Mutex<Option<Hold>>>,
}
/// What the writer hold saw.
#[derive(Clone, Copy)]
struct Hold {
    start: Instant,
    end: Instant,
    /// Whether the session's epoch-1 handoff boundary had already committed when the hold
    /// began. Writes are serialized, so `false` means the turn's write commits after the hold.
    boundary_before: bool,
}
impl HoldAfterBegin {
    fn hold_writer(&self, session: Id) -> Result<(), StorageError> {
        let held = self.held.clone();
        let (entered, admitted) = std::sync::mpsc::sync_channel(1);
        self.inner.submit_write(
            WriteOptions::fixture(),
            write_job(
                move |tx| {
                    let start = Instant::now();
                    let boundary_before = tx.sessions().epoch(&session, 1)?.is_some();
                    let _ = entered.send(());
                    std::thread::sleep(HOLD);
                    *held.lock().unwrap() = Some(Hold {
                        start,
                        end: Instant::now(),
                        boundary_before,
                    });
                    Ok(())
                },
                |_: Result<(), StorageError>| {},
            ),
        )?;
        admitted.recv().map_err(|_| StorageError::Cancelled)
    }
}
impl ApplicationStore for HoldAfterBegin {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn commit_signal(&self) -> Option<CommitSignal> {
        self.inner.commit_signal()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let armed = self.armed.lock().unwrap().clone();
        if let Some(session) = armed {
            let begun = read_snapshot(&*self.inner, &ReadOptions::default(), |snapshot| {
                Ok(snapshot.runs().session_turn(&session)?.is_some())
            })?;
            if begun && self.armed.lock().unwrap().take().is_some() {
                self.hold_writer(session)?;
            }
        }
        self.inner.read(options, work)
    }
}

/// The test dispatcher's store: records when each settlement write is submitted. After the
/// delivery begins, the only settlement writes of that dispatcher are its lease renewals.
struct Renewals {
    inner: Arc<dyn ApplicationStore>,
    submitted: Mutex<Vec<Instant>>,
}
impl ApplicationStore for Renewals {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        if options.class == WriteClass::EffectSettlement {
            self.submitted.lock().unwrap().push(Instant::now());
        }
        self.inner.submit_write(options, job)
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
}

/// A delivery that stays claimed until the test ends, so its lease keeps being renewed.
struct Parked(Arc<tokio::sync::Notify>);
impl AsyncDeliveryHandler for Parked {
    fn start(&self, _: AsyncDelivery) -> AsyncFuture {
        self.0.notify_one();
        Box::pin(std::future::pending::<AsyncOutcome>())
    }
}

#[test]
fn a_held_writer_during_an_agent_turn_does_not_stop_dispatcher_lease_renewal() {
    let _native_case = native_runtime_case();
    let h = Harness::new();
    let inner = h.open();
    seed(&h);
    let app_store = Arc::new(HoldAfterBegin {
        inner: inner.clone(),
        armed: Mutex::new(None),
        held: Arc::new(Mutex::new(None)),
    });
    let fx = Fx::over(h, app_store.clone());
    runtime().block_on(async {
        let app = runtime_with(&fx, Arc::new(Authority), Arc::new(DoneWire), |_| {}).await;
        let (context, agent) = pinned_agent_session(&fx);
        // A membership handoff committed before the turn: the session's new epoch has no
        // boundary yet, so the turn's first store work after `begin_turn` writes it.
        let db = rusqlite::Connection::open(fx.h.root.join("workspace.sqlite")).unwrap();
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        db.execute(
            "UPDATE assignments SET purpose='Child',generation=1,revision=revision+1 WHERE id=?1",
            [agent.assignment_id.0.as_str()],
        )
        .unwrap();
        db.execute(
            "UPDATE sessions SET context_epoch=1,revision=revision+1 WHERE id=?1",
            [context.session_id().unwrap().as_str()],
        )
        .unwrap();

        let renewals = Arc::new(Renewals {
            inner: inner.clone(),
            submitted: Mutex::new(Vec::new()),
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let mut parked = consumer("turn-runtime-parked", Recorder::ok());
        parked.async_handler = Some(Arc::new(Parked(started.clone())));
        let entry = enqueue(&*inner, "turn-runtime-parked", None, json!({}));
        let dispatcher = dispatcher_with(&fx.h, renewals.clone(), vec![parked], |c| {
            c.lease_ms = LEASE_MS;
            c.wake_jump_ms = i64::MAX;
        });
        within("the parked delivery is renewed", async {
            assert_eq!(dispatcher.tick().await.unwrap().claimed, 1);
            started.notified().await;
            let begun = renewals.submitted.lock().unwrap().len();
            // Start the turn right after a renewal, so the next one falls due inside the hold.
            while renewals.submitted.lock().unwrap().len() == begun {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert_eq!(entry_state(&*inner, &entry.id), QueueState::Claimed);

        *app_store.armed.lock().unwrap() = context.session_id().cloned();
        let (stop_tx, stop) = tokio::sync::watch::channel(false);
        let turn = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(
            context.clone(),
            agent.clone(),
            stop,
        ));
        let Hold {
            start,
            end,
            boundary_before,
        } = within("the writer hold ends", async {
            loop {
                if let Some(span) = *app_store.held.lock().unwrap() {
                    return span;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let during = renewals
            .submitted
            .lock()
            .unwrap()
            .iter()
            .filter(|at| **at > start && **at < end)
            .count();
        assert!(
            during >= 1,
            "the dispatcher renewed no lease while the writer was held for {:?}",
            end - start
        );
        // The turn's handoff boundary write waited behind the held writer: it had not committed
        // when the hold began, and it commits only after the hold.
        assert!(
            !boundary_before,
            "the turn's handoff boundary committed before the writer hold began"
        );
        let session = context.session_id().unwrap().clone();
        let written = within("the handoff boundary is written", async {
            while read(&*inner, |s| s.sessions().epoch(&session, 1)).is_none() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Instant::now()
        })
        .await;
        assert!(
            written >= end,
            "the handoff boundary was seen before the hold ended"
        );
        stop_tx.send(true).unwrap();
        within("the stopped turn ends", turn).await.unwrap();
        dispatcher.stop();
        app.shutdown().await;
    });
}

fn entry_state(store: &dyn ApplicationStore, entry: &Id) -> QueueState {
    common::entry(store, entry).state
}
