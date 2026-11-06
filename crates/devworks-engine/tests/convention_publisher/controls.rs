use super::*;
use devworks_core::{
    ports::files::{ArtifactFiles, FileFault, PublishedBlob},
    protocol::{ProjectId, RootId},
};
use std::{
    io::Read,
    path::Path,
    sync::{Mutex, mpsc},
};
type Gate = (mpsc::SyncSender<()>, mpsc::Receiver<()>);
struct GatedFiles {
    inner: NativeArtifactFiles,
    gate: Mutex<Option<Gate>>,
}
struct GatedReader {
    inner: Box<dyn Read + Send>,
    gate: Option<Gate>,
}
impl Read for GatedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Some((entered, release)) = self.gate.take() {
            entered.send(()).map_err(std::io::Error::other)?;
            release
                .recv_timeout(std::time::Duration::from_secs(3))
                .map_err(std::io::Error::other)?;
        }
        self.inner.read(out)
    }
}
impl ArtifactFiles for GatedFiles {
    fn publish(
        &self,
        s: &str,
        src: &mut dyn Read,
        d: &str,
        n: u64,
        r: Option<&str>,
        g: &str,
    ) -> Result<PublishedBlob, FileFault> {
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
    fn open_controlled(
        &self,
        d: &str,
        g: &str,
        n: u64,
        stopped: &mut dyn FnMut() -> bool,
    ) -> Result<Box<dyn Read + Send>, FileFault> {
        let reader = self.inner.open_controlled(d, g, n, stopped)?;
        let gate = self.gate.lock().unwrap().take();
        Ok(Box::new(GatedReader {
            inner: reader,
            gate,
        }))
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

fn fixture(root: &str) -> (Fixture, Arc<GatedFiles>) {
    let h = Harness::new();
    let store = h.open();
    common::seed_root(&h, root);
    let ctx = principal(&*store, "body-author", PrincipalKind::Human).with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(RootId(id(root))),
        None,
    );
    let files = Arc::new(GatedFiles {
        inner: NativeArtifactFiles::new(h.root.clone(), h.root.with_extension("cache")),
        gate: Mutex::new(None),
    });
    let artifacts = Arc::new(ArtifactService::new(
        files.clone(),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let index = Arc::new(IndexService::new(
        store.clone(),
        artifacts.clone(),
        h.ids.clone(),
        h.clock(),
    ));
    (
        Fixture {
            h,
            store,
            artifacts,
            index,
            ctx,
        },
        files,
    )
}
fn gate(files: &GatedFiles) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (entered, wait) = mpsc::sync_channel(1);
    let (release, blocked) = mpsc::sync_channel(1);
    *files.gate.lock().unwrap() = Some((entered, blocked));
    (wait, release)
}
fn pause(store: &dyn ApplicationStore, root: RootId) {
    write(store, move |tx| {
        let old = tx.runs().root_control(&root)?;
        let mut current = old
            .clone()
            .unwrap_or_else(|| StoredRootControl::initial(root));
        current.desired = DesiredExecution::Paused;
        if let Some(old) = old {
            current.revision += 1;
            tx.runs().update_root_control(&current, old.revision)
        } else {
            tx.runs().insert_root_control(&current)
        }
    })
    .unwrap();
}
#[test]
fn pause_after_actual_body_read_blocks_publication_without_clearing_hold_and_continue_can_reconcile()
 {
    let (f, files) = fixture("pause-root");
    let (owner, op) = f.source("paused-body", "# Policy\nexact body");
    let (wait, release) = gate(&files);
    let index = f.index.clone();
    let actual = op.clone();
    let worker = std::thread::spawn(move || index.run_index(&actual, &Cancellation::new(), None));
    wait.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    pause(&*f.store, RootId(id("pause-root")));
    release.send(()).unwrap();
    assert!(matches!(worker.join().unwrap(), Err(StorageError::Busy)));
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    let control = read(&*f.store, |r| {
        r.runs().root_control(&RootId(id("pause-root")))
    })
    .unwrap();
    assert_eq!(control.desired, DesiredExecution::Paused);
    assert_eq!(control.stop_epoch, 0);
    write(&*f.store, move |tx| {
        let mut resumed = control.clone();
        resumed.desired = DesiredExecution::Enabled;
        resumed.revision += 1;
        tx.runs().update_root_control(&resumed, control.revision)
    })
    .unwrap();
    assert!(matches!(
        f.index.run_index(&op, &Cancellation::new(), None).unwrap(),
        IndexOutcome::Completed(_)
    ));
}
#[test]
fn relinked_task_cannot_publish_under_old_parent_or_waive_new_parent_pause() {
    let (mut f, files) = fixture("feature-a");
    common::seed_root(&f.h, "feature-b");
    let connection = rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
    connection.execute("INSERT INTO work_items(id,project_id,kind,parent_feature_id,status_id,title,priority,manual_order,revision,scope_revision,created_at,brief_json)VALUES('task','project-a','task','feature-a',NULL,'Task',1,0,1,1,0,'{}')",[]).unwrap();
    drop(connection);
    f.ctx = f.ctx.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(RootId(id("task"))),
        None,
    );
    let (owner, op) = f.source("relinked-body", "authored body");
    let original = op.payload["controlRoots"].clone();
    let (wait, release) = gate(&files);
    let index = f.index.clone();
    let actual = op.clone();
    let worker = std::thread::spawn(move || index.run_index(&actual, &Cancellation::new(), None));
    wait.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    write(&*f.store, |tx| {
        let mut task = tx.work().work(&id("task"))?.unwrap();
        task.parent_feature_id = Some(id("feature-b"));
        task.revision += 1;
        task.scope_revision += 1;
        tx.work().update_work(&task, 1)
    })
    .unwrap();
    pause(&*f.store, RootId(id("feature-b")));
    release.send(()).unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        Err(StorageError::ScopeDenied)
    ));
    assert!(
        read(&*f.store, |r| r
            .conventions()
            .convention_index_head(&owner.id))
        .is_none()
    );
    assert_eq!(
        read(&*f.store, |r| r.operations().operation(&op.id))
            .unwrap()
            .payload["controlRoots"],
        original
    );
    assert!(
        original
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r[0] == "feature-a")
    );
    assert_eq!(
        read(&*f.store, |r| r
            .runs()
            .root_control(&RootId(id("feature-b"))))
        .unwrap()
        .desired,
        DesiredExecution::Paused
    );
}
