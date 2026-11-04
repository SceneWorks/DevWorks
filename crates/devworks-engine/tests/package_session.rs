//! A package protocol session with a helper that stopped reading its input: no request,
//! cancellation or failure waits on the helper's pipe, a frame stuck in the pipe is visible,
//! and an outbox the helper never drains fails the session instead of growing.
use devworks_engine::package_host::session::{Broker, Reply, Session, rpc_error};
use devworks_extension_protocol::package::{
    HostRequest, PackageRequest, RpcError, RpcErrorClass, RpcResult, RpcVersion, Shutdown,
};
use devworks_extension_protocol::scalars as wire;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Blocks every read and write until released (a helper that neither reads nor writes).
#[derive(Clone, Default)]
struct Stuck(Arc<(Mutex<bool>, Condvar)>);
impl Stuck {
    fn block(&self) {
        let (m, cv) = &*self.0;
        let mut released = m.lock().unwrap();
        while !*released {
            released = cv.wait(released).unwrap();
        }
    }
    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}
impl Write for Stuck {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        self.block();
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Read for Stuck {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        self.block();
        Ok(0)
    }
}

struct NoBroker;
impl Broker for NoBroker {
    fn handle(&self, _: &Session, _: &PackageRequest) -> Result<RpcResult, RpcError> {
        Err(rpc_error(RpcErrorClass::Unavailable, "none"))
    }
}

fn shutdown(id: String) -> HostRequest {
    HostRequest::Shutdown {
        jsonrpc: RpcVersion::V2,
        id,
        params: Shutdown {
            process_session_id: wire::Id::try_from("s-1".to_owned()).unwrap(),
            deadline_ms: 100,
        },
    }
}

#[test]
fn a_helper_that_stops_reading_never_blocks_requests_cancellation_or_failure() {
    let pipe = Stuck::default();
    let session = Session::start(
        "s-1".into(),
        Box::new(pipe.clone()),
        Box::new(pipe.clone()),
        Arc::new(NoBroker),
        4,
    );
    let (tx, rx) = mpsc::channel();
    let s = session.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        // The writer takes the first frame and blocks in the pipe; every later call returns
        // within its own deadline.
        let first = s.request(shutdown, Duration::from_millis(200));
        let second = s.request(shutdown, Duration::from_millis(200));
        let stalled = s.write_stalled(Duration::from_millis(100));
        s.fail("terminated");
        let after = s.request(shutdown, Duration::from_secs(5));
        tx.send((first, second, stalled, after, started.elapsed()))
            .unwrap();
    });
    let (first, second, stalled, after, took) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("a call blocked on a helper that stopped reading");
    for r in [first, second, after] {
        assert!(matches!(r, Reply::Lost(_)), "{r:?}");
    }
    assert!(stalled, "the stuck frame is visible");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(session.is_closed());
    pipe.release();
}

#[test]
fn an_outbox_the_helper_never_drains_fails_the_session() {
    let pipe = Stuck::default();
    let session = Session::start(
        "s-1".into(),
        Box::new(pipe.clone()),
        Box::new(pipe.clone()),
        Arc::new(NoBroker),
        4,
    );
    let (tx, rx) = mpsc::channel();
    let s = session.clone();
    std::thread::spawn(move || {
        // Each request is queued at once (no wait); the bounded outbox fills.
        for _ in 0..100_000 {
            let _ = s.request(shutdown, Duration::ZERO);
            if s.is_closed() {
                break;
            }
        }
        tx.send(s.failure()).unwrap();
    });
    let failure = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("queueing blocked on the helper");
    assert!(
        failure
            .as_deref()
            .is_some_and(|f| f.contains("not reading its input")),
        "{failure:?}"
    );
    pipe.release();
}
