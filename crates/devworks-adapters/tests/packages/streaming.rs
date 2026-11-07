//! Real native package files. The allocation probe is local to the calling test thread.
use super::*;
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::authority::reads::{ReadRequest, ReadRoot, VerifiedReader};
use devworks_core::ports::packages::{InspectedFile, MAX_BUNDLE_BYTES};
use sha2::{Digest, Sha256};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Read, Write};

#[derive(Clone, Copy)]
struct Probe {
    enabled: bool,
    largest: usize,
}
thread_local! {static PROBE:Cell<Probe>=const{Cell::new(Probe{enabled:false,largest:0})};}
struct Allocator;
fn allocated(size: usize) {
    let _ = PROBE.try_with(|probe| {
        let mut value = probe.get();
        if value.enabled && size > value.largest {
            value.largest = size;
            probe.set(value);
        }
    });
}
// SAFETY: allocation/deallocation use the unchanged System allocator and exact original
// layout/pointers. The nonallocating thread-local Cell only records requested allocation size.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
fn measured<T>(run: impl FnOnce() -> T) -> (T, usize) {
    struct Disable;
    impl Drop for Disable {
        fn drop(&mut self) {
            PROBE.with(|p| {
                let mut v = p.get();
                v.enabled = false;
                p.set(v);
            });
        }
    }
    PROBE.with(|p| {
        p.set(Probe {
            enabled: true,
            largest: 0,
        })
    });
    let guard = Disable;
    let result = run();
    let largest = PROBE.with(|p| p.get().largest);
    drop(guard);
    (result, largest)
}
struct Fixture {
    root: PathBuf,
    store: NativePackageFiles,
    files: Vec<InspectedFile>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    fn new(length: u64, other: bool) -> Self {
        let root = root();
        let store = NativePackageFiles::new(root.clone());
        let directory = store.directory("stream-package", &"a".repeat(64));
        std::fs::create_dir_all(&directory).unwrap();
        let mut file = std::fs::File::create(directory.join("help.md")).unwrap();
        let mut hash = Sha256::new();
        let chunk = [b'x'; 64 * 1024];
        let mut remaining = length;
        while remaining != 0 {
            let n = remaining.min(chunk.len() as u64) as usize;
            file.write_all(&chunk[..n]).unwrap();
            hash.update(&chunk[..n]);
            remaining -= n as u64;
        }
        file.sync_all().unwrap();
        let sha256 = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut files = vec![InspectedFile {
            path: "help.md".into(),
            sha256,
            byte_length: length,
            executable: false,
        }];
        if other {
            std::fs::create_dir_all(directory.join("bin")).unwrap();
            let body = vec![b'y'; 4 * 64 * 1024];
            std::fs::write(directory.join("bin/tool"), &body).unwrap();
            files.push(InspectedFile {
                path: "bin/tool".into(),
                sha256: sha256_hex(&body),
                byte_length: body.len() as u64,
                executable: true,
            });
        }
        Self { root, store, files }
    }
    fn open(
        &self,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<devworks_core::ports::packages::VerifiedPackageFile, PackageFileError> {
        self.store.open_verified(
            "stream-package",
            &"a".repeat(64),
            &self.files[0],
            &self.files,
            MAX_BUNDLE_BYTES,
            stop,
        )
    }
    fn path(&self, file: &str) -> PathBuf {
        self.store.path("stream-package", &"a".repeat(64), file)
    }
}

#[test]
fn native_full_bundle_limit_guide_and_ordinary_verification_never_allocate_its_body() {
    let fixture = Fixture::new(MAX_BUNDLE_BYTES, false);
    let (verified, allocation) = measured(|| {
        fixture
            .store
            .verify("stream-package", &"a".repeat(64), &fixture.files)
    });
    verified.unwrap();
    eprintln!(
        "ordinary package verify largest allocation: {allocation} bytes for {MAX_BUNDLE_BYTES} bytes"
    );
    assert!(
        allocation < 1024 * 1024,
        "ordinary verify allocated {allocation} bytes"
    );
    let (opened, allocation) = measured(|| fixture.open(&mut || false));
    let mut opened = opened.unwrap();
    eprintln!(
        "guide firsthash largest allocation: {allocation} bytes for {MAX_BUNDLE_BYTES} bytes"
    );
    assert!(
        allocation < 1024 * 1024,
        "guide firsthash allocated {allocation} bytes"
    );
    assert_eq!(opened.byte_length, MAX_BUNDLE_BYTES);
    for _ in 0..2 {
        let mut bytes = 0u64;
        let mut hash = Sha256::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let n = opened.reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            bytes += n as u64;
            hash.update(&chunk[..n]);
        }
        assert_eq!(bytes, MAX_BUNDLE_BYTES);
        assert_eq!(
            hash.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            opened.sha256
        );
        opened.reader.rewind().unwrap();
    }
}

#[test]
fn native_guide_firsthash_other_file_hash_and_directory_checks_honor_control() {
    let fixture = Fixture::new(4 * 64 * 1024 + 13, true);
    for limit in [1, 3, 12, 20] {
        let mut calls = 0;
        let error = fixture
            .open(&mut || {
                calls += 1;
                calls >= limit
            })
            .err()
            .expect("verification stopped");
        assert!(
            matches!(error,PackageFileError::Io(ref text)if text.contains("interrupted")),
            "{error:?}"
        );
        assert_eq!(calls, limit);
    }
    fixture.open(&mut || false).unwrap();
    std::fs::write(fixture.path("bin/tool"), vec![b'z'; 4 * 64 * 1024]).unwrap();
    assert!(matches!(
        fixture.open(&mut || false),
        Err(PackageFileError::Mismatch(_))
    ));
}

#[test]
fn native_rewindable_reads_reapply_original_bound_on_every_rewind() {
    let fixture = Fixture::new(9, false);
    let fs = devworks_adapters::fs::NativeFilesystem::new();
    let directory = fixture.store.directory("stream-package", &"a".repeat(64));
    let resolved = fs.resolve(directory.to_str().unwrap(), None).unwrap();
    let root = ReadRoot {
        location: resolved.entry.path,
        identity: resolved.entry.identity,
    };
    let request = ReadRequest {
        root: &root,
        relative: "help.md",
        max_bytes: 3,
        allow_hard_links: false,
    };
    let mut opened = fs.open_rewindable(&request).unwrap();
    for _ in 0..3 {
        let mut bytes = Vec::new();
        opened.reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"xxxx");
        opened.reader.rewind().unwrap();
    }
    let mut ordinary = fs.open(&request).unwrap();
    let mut bytes = Vec::new();
    ordinary.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"xxxx");
}

#[test]
fn native_verified_stream_refuses_tamper_links_and_extra_entries() {
    let fixture = Fixture::new(16, true);
    assert!(
        fixture
            .store
            .open_verified(
                "stream-package",
                &"a".repeat(64),
                &fixture.files[0],
                &fixture.files,
                15,
                &mut || false
            )
            .is_err()
    );
    std::fs::write(fixture.path("help.md"), [b'x'; 17]).unwrap();
    assert!(fixture.open(&mut || false).is_err());
    std::fs::write(fixture.path("help.md"), b"wrong bytes here!").unwrap();
    assert!(fixture.open(&mut || false).is_err());
    std::fs::write(fixture.path("help.md"), [b'x'; 16]).unwrap();
    fixture.open(&mut || false).unwrap();
    let external = fixture.root.join("outside");
    std::fs::write(&external, [b'x'; 16]).unwrap();
    std::fs::remove_file(fixture.path("help.md")).unwrap();
    std::fs::hard_link(&external, fixture.path("help.md")).unwrap();
    assert!(fixture.open(&mut || false).is_err());
    assert!(
        fixture
            .store
            .verify("stream-package", &"a".repeat(64), &fixture.files)
            .is_err()
    );
    std::fs::remove_file(fixture.path("help.md")).unwrap();
    std::fs::write(fixture.path("help.md"), [b'x'; 16]).unwrap();
    #[cfg(unix)]
    {
        std::fs::remove_file(fixture.path("help.md")).unwrap();
        std::os::unix::fs::symlink(&external, fixture.path("help.md")).unwrap();
        assert!(fixture.open(&mut || false).is_err());
        std::fs::remove_file(fixture.path("help.md")).unwrap();
        std::fs::write(fixture.path("help.md"), [b'x'; 16]).unwrap();
    }
    std::fs::create_dir(fixture.path("extra-empty-directory")).unwrap();
    assert!(fixture.open(&mut || false).is_err());
    std::fs::remove_dir(fixture.path("extra-empty-directory")).unwrap();
    std::fs::remove_file(fixture.path("help.md")).unwrap();
    assert!(matches!(
        fixture.open(&mut || false),
        Err(PackageFileError::Missing(_))
    ));
    assert!(matches!(
        fixture
            .store
            .verify("stream-package", &"a".repeat(64), &fixture.files),
        Err(PackageFileError::Missing(_))
    ));
}

#[cfg(unix)]
#[test]
fn native_rewind_keeps_the_verified_handle_after_path_replacement() {
    let fixture = Fixture::new(16, false);
    let mut opened = fixture.open(&mut || false).unwrap();
    std::fs::rename(fixture.path("help.md"), fixture.path("old.md")).unwrap();
    std::fs::write(fixture.path("help.md"), b"replacement body").unwrap();
    for _ in 0..2 {
        let mut bytes = Vec::new();
        opened.reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, [b'x'; 16]);
        opened.reader.rewind().unwrap();
    }
    assert!(fixture.open(&mut || false).is_err());
}

#[test]
fn rewindable_open_never_falls_back_to_a_buffered_read() {
    struct OnlyRead(AtomicU64);
    impl VerifiedReader for OnlyRead {
        fn read(
            &self,
            _: &ReadRequest<'_>,
        ) -> Result<
            devworks_core::authority::reads::VerifiedBytes,
            devworks_core::authority::reads::ReadRefusal,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            unreachable!("rewindable default must not buffer")
        }
    }
    let reader = OnlyRead(AtomicU64::new(0));
    let root = ReadRoot {
        location: PathBuf::from("unused"),
        identity: None,
    };
    assert!(matches!(
        reader.open_rewindable(&ReadRequest {
            root: &root,
            relative: "help.md",
            max_bytes: 1,
            allow_hard_links: false
        }),
        Err(devworks_core::authority::reads::ReadRefusal::Unsupported(_))
    ));
    assert_eq!(reader.0.load(Ordering::SeqCst), 0);
}
