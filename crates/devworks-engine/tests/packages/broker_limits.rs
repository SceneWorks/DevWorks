//! What the broker returns to a helper and what a session retains: a host-side failure reaches
//! the package as its typed class with a fixed message (the detail stays in the host's journal),
//! a finished invocation is forgotten entirely, and produced events beyond the kept bound are
//! counted.
use super::broker::{application, input_schema, run_calls};
use super::*;
use devworks_core::ports::storage::Cancellation;
use devworks_core::protocol::OperationState;
use devworks_core::registry::Mode;
use devworks_engine::package_host::broker::APPLICATION_CAPABILITY;
use devworks_engine::package_host::session::{
    Broker, EVENTS_KEPT, Reply, Session, Slot, rpc_error,
};
use devworks_extension_protocol::package::{
    HostRequest, PackageRequest, RpcError, RpcErrorClass, RpcResult,
};
use std::io::Read;
use std::sync::mpsc;

#[test]
fn a_host_side_failure_reaches_the_package_as_its_class_and_a_fixed_message() {
    let _s = serial();
    let a = App::new();
    // Per process: concurrent copies of this binary never share the fixture's script file.
    let name = format!("fault-pack-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    package(a.install(&a.user, &spec));
    a.ready(&name);
    // This composition registers no indexing use case: the gateway fails the query.
    let args = a.save(
        &a.user,
        br#"{"scope":{"kind":"Workspace"},"roots":[],"page":{"limit":50}}"#,
    );
    let mut step = json!({"actionKey": "list", "call": application("indexing.discover",
        input_schema("indexing.discover", Mode::Query, None), json!("$input"))});
    step["reportMessage"] = json!(true);
    let (progress, _) = run_calls(
        &a,
        &name,
        json!({"calls": [step]}),
        &args,
        Some(a.agent.clone()),
        vec![],
    );
    assert_eq!(
        progress,
        vec!["call:list:Unavailable:the query could not be run"]
    );
    // The host keeps the detail in the call's journal.
    let calls = read(&*a.store, |s| {
        s.operations().operations_for_capability(
            APPLICATION_CAPABILITY,
            &[OperationState::Failed],
            None,
            10,
        )
    });
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        format!("{:?}", calls[0]).contains("no use case is registered for indexing.discover"),
        "{calls:?}"
    );
}

/// The helper's output: the frames a test feeds, then end of stream once the sender drops.
struct Feed {
    frames: mpsc::Receiver<Vec<u8>>,
    pending: Vec<u8>,
}
impl Read for Feed {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending.is_empty() {
            match self.frames.recv() {
                Ok(bytes) => self.pending = bytes,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

struct NoBroker;
impl Broker for NoBroker {
    fn handle(&self, _: &Session, _: &PackageRequest) -> Result<RpcResult, RpcError> {
        Err(rpc_error(RpcErrorClass::Unavailable, "none"))
    }
}

fn framed(v: Value) -> Vec<u8> {
    let mut out = Vec::new();
    devworks_extension_protocol::frame::write_frame(&mut out, v.to_string().as_bytes()).unwrap();
    out
}

/// A session over a fed helper output with one invocation `i-1` (token `t-1`) sent.
fn invoked() -> (Arc<Session>, Arc<Slot>, mpsc::Sender<Vec<u8>>) {
    let (tx, rx) = mpsc::channel();
    let feed = Feed {
        frames: rx,
        pending: Vec::new(),
    };
    let session = Session::start(
        "s-1".into(),
        Box::new(feed),
        Box::new(std::io::sink()),
        Arc::new(NoBroker),
        4,
    );
    let slot = Arc::new(Slot {
        invocation_id: "i-1".into(),
        token: "t-1".into(),
        package_id: "p".into(),
        contribution_id: "p/c".into(),
        revoked: AtomicBool::new(false),
        cancellation: Cancellation::default(),
        state: Mutex::new(Default::default()),
        changed: Condvar::new(),
        context: Box::new(()),
    });
    session
        .invoke(slot.clone(), |id| {
            serde_json::from_value::<HostRequest>(json!({"jsonrpc": "2.0", "id": id,
                "method": "host.invoke", "params": {"processSessionId": "s-1",
                "invocationId": "i-1", "invocationToken": "t-1", "contributionId": "p/c",
                "scope": {"kind": "Workspace"}, "allowedOperations": [], "registryRevision": "1",
                "stateGeneration": "1", "authorityRevision": "1", "controlGeneration": "1",
                "operationId": "op-1", "deadlineMs": 1000, "input": {"artifactId": "a-1",
                "versionId": "v1", "sha256": "e".repeat(64), "byteLength": "4",
                "mediaType": "application/json"}}}))
            .unwrap()
        })
        .unwrap();
    (session, slot, tx)
}

#[test]
fn a_finished_invocation_is_forgotten_even_when_its_session_ended_it() {
    let (session, slot, helper) = invoked();
    // The helper ends without answering: the session ends the invocation `Lost`.
    drop(helper);
    assert!(matches!(
        slot.wait(Duration::from_secs(10)),
        Some(Reply::Lost(_))
    ));
    assert!(session.holds("i-1"));
    session.forget("i-1");
    // Nothing of the session still holds the finished invocation. (Its reference count is no
    // evidence: the reader thread that ended the slot may still hold a clone for a moment.)
    assert!(!session.holds("i-1"));
}

#[test]
fn produced_events_beyond_the_kept_bound_are_counted() {
    let (session, slot, helper) = invoked();
    for n in 0..EVENTS_KEPT + 3 {
        helper
            .send(framed(
                json!({"jsonrpc": "2.0", "method": "package.event", "params": {
                "processSessionId": "s-1", "invocationId": "i-1", "invocationToken": "t-1",
                "sequence": (n + 1).to_string(),
                "event": {"kind": "Verification", "passed": true, "evidence": []}}}),
            ))
            .unwrap();
    }
    let until = Instant::now() + PATIENCE;
    loop {
        let (kept, dropped) = {
            let s = slot.state.lock().unwrap();
            (s.events.len(), s.dropped_events)
        };
        if kept + dropped as usize == EVENTS_KEPT + 3 {
            assert_eq!((kept, dropped), (EVENTS_KEPT, 3));
            break;
        }
        assert!(
            Instant::now() < until && !session.is_closed(),
            "kept {kept}, dropped {dropped}, failure {:?}",
            session.failure()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
