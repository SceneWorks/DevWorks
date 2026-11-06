//! Real native bytes and SQLite remain intact across transient file-port failures.
use super::*;
use devworks_core::ports::files::{ArtifactFiles, FileFault, PublishedBlob};
use std::{io::Read, path::Path};
struct FaultFiles {
    inner: NativeArtifactFiles,
    target: Mutex<Option<String>>,
    mode: AtomicUsize,
    hits: Arc<AtomicUsize>,
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
impl ArtifactFiles for FaultFiles {
    fn publish(
        &self,
        name: &str,
        source: &mut dyn Read,
        digest: &str,
        bytes: u64,
        reuse: Option<&str>,
        generation: &str,
    ) -> Result<PublishedBlob, FileFault> {
        self.inner
            .publish(name, source, digest, bytes, reuse, generation)
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
    fn open_controlled(
        &self,
        d: &str,
        g: &str,
        n: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let target = self.target.lock().unwrap().as_deref() == Some(d);
        let mode = if target {
            self.mode.load(Ordering::SeqCst)
        } else {
            0
        };
        if mode == 1 {
            self.hits.fetch_add(1, Ordering::SeqCst);
            return Err(FileFault::Io("injected transient open failure".into()));
        }
        let reader = self.inner.open_controlled(d, g, n, stopped)?;
        if mode == 2 {
            Ok(Box::new(ReadFault {
                inner: reader,
                prefix: true,
                hits: self.hits.clone(),
            }))
        } else {
            Ok(reader)
        }
    }

    fn read_range_controlled(
        &self,
        d: &str,
        g: &str,
        n: u64,
        o: u64,
        m: usize,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Vec<u8>, FileFault> {
        self.inner.read_range_controlled(d, g, n, o, m, stopped)
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
    fn export(&self, d: &str, g: &str, n: u64, p: &Path, o: bool) -> Result<(), FileFault> {
        self.inner.export(d, g, n, p, o)
    }
}
fn transient_help_fault_case(
    rt: &tokio::runtime::Runtime,
    app: &HelpApp,
    files: &FaultFiles,
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
    rt.block_on(within("transient Help fault remains recoverable", async {
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
    }));
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
    let rt = runtime();
    let h = Harness::new();
    let files = Arc::new(FaultFiles {
        inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
        target: Mutex::new(None),
        mode: AtomicUsize::new(0),
        hits: Arc::new(AtomicUsize::new(0)),
    });
    let app = boot_help_with_files(&rt, &h, files.clone());
    let _user = principal(&*app.store, "user-a", PrincipalKind::Human);
    for (generated, mode) in [(true, 1), (true, 2), (false, 1)] {
        transient_help_fault_case(&rt, &app, &files, generated, mode);
    }
    finish(&rt, app);
}
