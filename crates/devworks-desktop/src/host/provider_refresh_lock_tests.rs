//! Actual Human configure owns the SQLite writer while Engine Drop closes OAuth publication.
use super::*;
use devworks_core::ports::storage::*;
use std::sync::Condvar;

struct Gate {
    entered: mpsc::Sender<()>,
    open: Mutex<bool>,
    changed: Condvar,
}
struct BlockedJob {
    job: Box<dyn WriteJob>,
    gate: Arc<Gate>,
}
impl WriteJob for BlockedJob {
    fn run(&mut self, tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
        let _ = self.gate.entered.send(());
        let mut open = self.gate.open.lock().unwrap();
        while !*open {
            open = self.gate.changed.wait(open).unwrap();
        }
        self.job.run(tx)
    }
    fn finish(self: Box<Self>, outcome: Result<(), StorageError>) {
        self.job.finish(outcome);
    }
}
struct ConfigureStore {
    inner: Arc<dyn ApplicationStore>,
    gate: Arc<Gate>,
}
impl ApplicationStore for ConfigureStore {
    fn info(&self) -> StoreInfo {
        self.inner.info()
    }
    fn read(
        &self,
        options: &ReadOptions,
        work: &mut dyn FnMut(&dyn ReadSnapshot) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        self.inner.read(options, work)
    }
    fn submit_write(
        &self,
        options: WriteOptions,
        job: Box<dyn WriteJob>,
    ) -> Result<(), StorageError> {
        self.inner.submit_write(
            options,
            Box::new(BlockedJob {
                job,
                gate: self.gate.clone(),
            }),
        )
    }
}
pub(super) struct BlockedConfigure {
    gate: Arc<Gate>,
    pub finished: mpsc::Receiver<()>,
}
impl BlockedConfigure {
    pub fn release(&self) {
        *self.gate.open.lock().unwrap() = true;
        self.gate.changed.notify_all();
    }
}
pub(super) fn block_configure(host: &Host) -> BlockedConfigure {
    let (entered, started) = mpsc::channel();
    let gate = Arc::new(Gate {
        entered,
        open: Mutex::new(false),
        changed: Condvar::new(),
    });
    let store = ConfigureStore {
        inner: host.runtime.store().clone(),
        gate: gate.clone(),
    };
    let context = host.ctx.clone();
    let authority = host.authority.clone();
    let ids = host.ids.clone();
    let connections = host.connections.clone();
    let (finished, done) = mpsc::channel();
    std::thread::spawn(move || {
        let gateway = devworks_core::gateway::Gateway {
            authority: authority.as_ref(),
            store: &store,
            ids: ids.as_ref(),
        };
        let current = read_snapshot(&store, &ReadOptions::default(), |s| {
            s.router().connection(&Id::new("usage-connection").unwrap())
        })
        .unwrap()
        .unwrap();
        let frame = serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":2,"requestKey":"blocked-native-configure","body":{"kind":"router.configure","action":{"kind":"Connection","connectionId":current.id,"expectedRevision":current.revision.to_string(),"name":"Concurrent actual configure","profile":current.profile,"endpoint":current.endpoint}}});
        let result = UseCases::new().with_connections(connections).command(
            &gateway,
            &context,
            "router.configure",
            &serde_json::to_vec(&frame).unwrap(),
        );
        assert!(
            !matches!(result, Ok(devworks_core::protocol::CommandReply::Recorded {receipt,..}) if matches!(receipt.decision, devworks_core::protocol::results::DomainDecision::Applied {..})),
            "a configure queued before host close must be cancelled without publication"
        );
        let _ = finished.send(());
    });
    started
        .recv_timeout(Duration::from_secs(5))
        .expect("actual configure did not own SQLite writer");
    BlockedConfigure {
        gate,
        finished: done,
    }
}
#[test]
fn configure_writer_and_oauth_refresh_do_not_deadlock_actual_engine_drop() {
    super::provider_refresh_shutdown_tests::run_case(true);
}
