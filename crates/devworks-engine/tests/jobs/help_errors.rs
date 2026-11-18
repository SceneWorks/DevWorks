//! Real native bytes and SQLite remain intact across transient file-port failures.
use super::*;
use devworks_core::ports::files::{ArtifactFiles, FileFault};
use devworks_test_support::{FaultFiles, FileOp};
use std::io::Read;
/// Transient faults on one blob's controlled reads, for the target digest only: mode 1 fails the
/// open, mode 2 lets a short prefix through and then fails the read. `files` is the store to compose.
struct TransientFaults {
    inner: Arc<NativeArtifactFiles>,
    files: Arc<FaultFiles>,
    target: Mutex<Option<String>>,
    mode: AtomicUsize,
    hits: Arc<AtomicUsize>,
}
impl TransientFaults {
    fn new(h: &Harness) -> Arc<Self> {
        let inner = Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        ));
        let faults = Arc::new(Self {
            files: FaultFiles::over(inner.clone()),
            inner,
            target: Mutex::new(None),
            mode: AtomicUsize::new(0),
            hits: Arc::new(AtomicUsize::new(0)),
        });
        let this = Arc::downgrade(&faults);
        faults.files.before(FileOp::OpenControlled, {
            let this = this.clone();
            move |call| match this.upgrade().map(|f| f.mode_for(call.digest)) {
                Some(1) => {
                    this.upgrade().unwrap().hits.fetch_add(1, Ordering::SeqCst);
                    Err(FileFault::Io("injected transient open failure".into()))
                }
                _ => Ok(()),
            }
        });
        faults
            .files
            .wrap_reader(FileOp::OpenControlled, move |call, reader| {
                match this.upgrade() {
                    Some(f) if f.mode_for(call.digest) == 2 => Box::new(ReadFault {
                        inner: reader,
                        prefix: true,
                        hits: f.hits.clone(),
                    }),
                    _ => reader,
                }
            });
        faults
    }
    fn mode_for(&self, digest: &str) -> usize {
        if self.target.lock().unwrap().as_deref() == Some(digest) {
            self.mode.load(Ordering::SeqCst)
        } else {
            0
        }
    }
}
struct ReadFault {
    inner: Box<dyn Read + Send>,
    prefix: bool,
    hits: Arc<AtomicUsize>,
}
impl Read for ReadFault {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.prefix {
            self.prefix = false;
            let length = out.len().min(8);
            self.inner.read(&mut out[..length])
        } else {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("injected transient read failure"))
        }
    }
}
fn transient_help_fault_case(
    rt: &tokio::runtime::Runtime,
    app: &HelpApp,
    files: &TransientFaults,
    generated: bool,
    mode: usize,
) {
    let topic_id = if generated {
        id(&format!(
            "capability-{}",
            &devworks_core::fingerprint::sha256_hex(b"help.search\0Query")[..40]
        ))
    } else {
        id("help")
    };
    let old_head = app.head();
    let old = read(&*app.store, |s| {
        s.help()
            .help_topic(old_head.generation_id.as_ref().unwrap(), &topic_id)
    })
    .unwrap();
    let reference: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(old.content_ref.clone().unwrap()).unwrap();
    let version = read(&*app.store, |s| {
        s.artifacts().version(&reference.version_id)
    })
    .unwrap();
    let original = {
        let mut source = files
            .inner
            .open(
                reference.sha256.as_str(),
                &version.blob_generation,
                reference.byte_length.get() as u64,
            )
            .unwrap();
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        bytes
    };
    let hits_before = files.hits.load(Ordering::SeqCst);
    *files.target.lock().unwrap() = Some(reference.sha256.as_str().into());
    files.mode.store(mode, Ordering::SeqCst);
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let reply = app.command(DOCS, serde_json::to_value(args).unwrap(), 1);
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { operation_id },
                ..
            },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    rt.block_on(durable(
        "transient Help fault remains recoverable",
        || app.ids.issued(),
        async {
            loop {
                app.runtime.dispatcher().run_until_idle().await.unwrap();
                let op = read(&*app.store, |s| s.operations().operation(&operation_id)).unwrap();
                if matches!(
                    op.state,
                    OperationState::OutcomeUnknown
                        | OperationState::Failed
                        | OperationState::ObservedComplete
                ) {
                    assert_eq!(
                        op.state,
                        OperationState::OutcomeUnknown,
                        "generated={generated}, mode={mode}: {op:?}"
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        },
    ));
    assert!(
        files.hits.load(Ordering::SeqCst) > hits_before,
        "the actual source path was faulted"
    );
    let head = app.head();
    assert_eq!(head.revision, old_head.revision);
    assert_eq!(head.generation_id, old_head.generation_id);
    assert_eq!(head.source_digest, old_head.source_digest);
    assert_eq!(
        read(&*app.store, |s| s.help().help_topic(
            old_head.generation_id.as_ref().unwrap(),
            &topic_id
        ))
        .unwrap(),
        old,
        "no manual-loss replacement or reference was published"
    );
    let retained = {
        let mut source = files
            .inner
            .open(
                reference.sha256.as_str(),
                &version.blob_generation,
                reference.byte_length.get() as u64,
            )
            .unwrap();
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes).unwrap();
        bytes
    };
    assert_eq!(
        retained, original,
        "the native original bytes remained present throughout the transient failure"
    );
    files.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        rt.block_on(app.runtime.dispatcher().observe_unknown())
            .unwrap(),
        1,
        "recovery observes the same accepted operation"
    );
    assert_eq!(
        read(&*app.store, |s| s.operations().operation(&operation_id))
            .unwrap()
            .state,
        OperationState::ObservedComplete
    );
    let recovered = app.head();
    assert_eq!(recovered.revision, old_head.revision);
    assert_eq!(recovered.generation_id, old_head.generation_id);
    assert_eq!(
        read(&*app.store, |s| s.help().help_topic(
            recovered.generation_id.as_ref().unwrap(),
            &topic_id
        ))
        .unwrap(),
        old,
        "clearing the fault retains the exact original content and reference"
    );
}

#[test]
fn transient_help_open_and_mid_read_faults_preserve_source_and_recover_the_same_operations() {
    let _native_case = crate::common::native_runtime_case();
    let rt = runtime();
    let h = Harness::new();
    let files = TransientFaults::new(&h);
    let app = boot_help_with_files(&rt, &h, files.files.clone());
    let _user = principal(&*app.store, "user-a", PrincipalKind::Human);
    for (generated, mode) in [(true, 1), (true, 2), (false, 1)] {
        transient_help_fault_case(&rt, &app, &files, generated, mode);
    }
    repair_same_source_bundle(&rt, &app, &files);
    finish(&rt, app);
}

// Version references name a digest; SQLite resolves its current physical blob
// generation. Repairing identical bundled bytes must preserve readable history.
fn repair_same_source_bundle(rt: &tokio::runtime::Runtime, app: &HelpApp, files: &TransientFaults) {
    let before = app.head();
    let topic = read(&*app.store, |s| {
        s.help()
            .help_topic(before.generation_id.as_ref().unwrap(), &id("help"))
    })
    .unwrap();
    let reference: devworks_core::protocol::provider::ArtifactRef =
        serde_json::from_value(topic.content_ref.clone().unwrap()).unwrap();
    let version = read(&*app.store, |s| {
        s.artifacts().version(&reference.version_id)
    })
    .unwrap();
    let old_page = result(query(
        app,
        &app.user(),
        "help.read",
        json!({"topicId":"help","version":topic.version,"maxChars":"65536"}),
        3,
    ));
    assert_eq!(old_page["kind"], "Available");
    files
        .inner
        .unlink(reference.sha256.as_str(), &version.blob_generation)
        .unwrap();
    assert!(
        !files
            .inner
            .present(
                reference.sha256.as_str(),
                &version.blob_generation,
                reference.byte_length.get() as u64
            )
            .unwrap()
    );
    let args = read(
        &*app.store,
        devworks_core::services::help::current_generation_args,
    );
    let reply = app.command(DOCS, serde_json::to_value(args).unwrap(), 1);
    let CommandReply::Recorded {
        receipt:
            devworks_core::protocol::Receipt {
                decision: DomainDecision::Accepted { operation_id },
                ..
            },
        ..
    } = reply
    else {
        panic!("{reply:?}")
    };
    rt.block_on(durable(
        "repair identical bundled Help source",
        || app.ids.issued(),
        async {
            loop {
                app.runtime.dispatcher().run_until_idle().await.unwrap();
                let op = read(&*app.store, |s| s.operations().operation(&operation_id)).unwrap();
                if is_terminal(op.state) {
                    assert_eq!(op.state, OperationState::ObservedComplete, "{op:?}");
                    break;
                }
                tokio::task::yield_now().await;
            }
        },
    ));
    let after = app.head();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.generation_id, before.generation_id);
    assert_eq!(after.source_digest, before.source_digest);
    assert_eq!(
        read(&*app.store, |s| s
            .help()
            .help_topic(&topic.generation_id, &topic.id))
        .unwrap(),
        topic
    );
    let repaired = read(&*app.store, |s| {
        s.artifacts().version(&reference.version_id)
    })
    .unwrap();
    assert_ne!(repaired.blob_generation, version.blob_generation);
    assert_eq!(
        result(query(
            app,
            &app.user(),
            "help.read",
            json!({"topicId":"help","version":topic.version,"maxChars":"65536"}),
            3
        ))["passage"]["text"],
        old_page["passage"]["text"]
    );
    assert!(matches!(
        artifact_read(app, &app.user(), topic.content_ref.as_ref().unwrap()),
        DomainQueryReply::Result { .. }
    ));
}
