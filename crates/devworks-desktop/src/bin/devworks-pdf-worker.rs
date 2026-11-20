//! DevWorks distribution sidecar. Its sole document authority is the inherited, unnamed pipe.
//! It runs with a hard Rust allocation budget independent of the app's allocator.
use devworks_core::services::documents::{
    extraction::ExtractionControl,
    pdf::{INPUT_LIMIT, PdfOperation, READY_MAGIC, REQUEST_MAGIC, WORKER_ARGUMENT, WORKER_SECONDS},
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs::File,
    io::{self, BufWriter, Read, Write},
    process::ExitCode,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

// Leave 64 MiB headroom for the mapped code, stack, and platform/runtime overhead. The
// dependency closure uses Rust allocations for PDF and image decoding.
const HEAP_LIMIT: usize = 448 * 1024 * 1024;

struct BoundedHeap(AtomicUsize);

impl BoundedHeap {
    fn reserve(&self, size: usize) -> bool {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(size).filter(|total| *total <= HEAP_LIMIT)
            })
            .is_ok()
    }

    fn free(&self, size: usize) {
        self.0.fetch_sub(size, Ordering::Relaxed);
    }
}

// SAFETY: allocation and layout ownership remain with System. The guard only accounts
// requested bytes before forwarding the exact allocator operation.
unsafe impl GlobalAlloc for BoundedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let pointer = unsafe { System.alloc(layout) };
        if pointer.is_null() {
            self.free(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        self.free(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // Account both allocations because System may move the memory.
        if !self.reserve(size) {
            return std::ptr::null_mut();
        }
        let new_pointer = unsafe { System.realloc(pointer, layout, size) };
        self.free(if new_pointer.is_null() {
            size
        } else {
            layout.size()
        });
        new_pointer
    }
}

#[global_allocator]
static HEAP: BoundedHeap = BoundedHeap(AtomicUsize::new(0));

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == ["--check-pdf-worker-memory"] {
        return if Vec::<u8>::new().try_reserve_exact(HEAP_LIMIT + 1).is_err() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let Some((mode, handles)) = args.split_first() else {
        return ExitCode::FAILURE;
    };
    let fault = match mode.to_str() {
        Some(WORKER_ARGUMENT) => None,
        #[cfg(feature = "test-support")]
        Some("--test-pdf-crash") => Some(Fault::Crash),
        #[cfg(feature = "test-support")]
        Some("--test-pdf-stall") => Some(Fault::Stall),
        _ => return ExitCode::FAILURE,
    };
    match open_channel(handles).and_then(|(input, output)| serve(input, output, fault)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

enum Fault {
    #[cfg(feature = "test-support")]
    Crash,
    #[cfg(feature = "test-support")]
    Stall,
}

fn serve(mut input: File, mut output: File, fault: Option<Fault>) -> io::Result<()> {
    output.write_all(&READY_MAGIC)?;
    let mut header = [0; 12];
    input.read_exact(&mut header)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "invalid PDF worker request");
    if header[..4] != REQUEST_MAGIC {
        return Err(bad());
    }
    let number = u32::from_le_bytes(header[4..8].try_into().expect("fixed wire header"));
    let operation = PdfOperation::from_number(number).map_err(|_| bad())?;
    let length = u32::from_le_bytes(header[8..].try_into().expect("fixed wire header")) as usize;
    if length > INPUT_LIMIT {
        return Err(bad());
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    if input.read(&mut [0])? != 0 {
        return Err(bad());
    }
    if let Some(fault) = fault {
        match fault {
            #[cfg(feature = "test-support")]
            Fault::Crash => {
                #[cfg(unix)]
                // SAFETY: this test child disables its own core dump before aborting.
                unsafe {
                    let limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &limit);
                }
                std::process::abort();
            }
            #[cfg(feature = "test-support")]
            Fault::Stall => {
                output.write_all(b"STAL")?;
                output.flush()?;
                std::thread::sleep(Duration::from_secs(WORKER_SECONDS));
            }
        }
    }
    let control = ExtractionControl {
        deadline: Instant::now() + Duration::from_secs(WORKER_SECONDS),
        cancelled: &|| false,
    };
    let reply = devworks_adapters::documents::pdf::process(bytes, operation, &control);
    let bounded = BoundedOutput {
        output,
        remaining: operation.wire_limit(),
    };
    let mut buffered = BufWriter::with_capacity(64 * 1024, bounded);
    serde_json::to_writer(&mut buffered, &reply).map_err(io::Error::other)?;
    buffered.flush()
}

struct BoundedOutput {
    output: File,
    remaining: usize,
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("PDF reply limit"));
        }
        let done = self.output.write(bytes)?;
        self.remaining -= done;
        Ok(done)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[cfg(unix)]
fn open_channel(handles: &[std::ffi::OsString]) -> io::Result<(File, File)> {
    use std::os::fd::FromRawFd;
    if !handles.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unexpected pipe arguments",
        ));
    }
    let valid = |fd| {
        let mut state = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat initializes the record on success. Check that the inherited descriptor
        // names a pipe. No path or arbitrary inherited resource is accepted.
        unsafe {
            libc::fstat(fd, state.as_mut_ptr()) == 0
                && state.assume_init().st_mode & libc::S_IFMT == libc::S_IFIFO
        }
    };
    if !valid(3) || !valid(4) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private pipe missing",
        ));
    }
    Ok(unsafe { (File::from_raw_fd(3), File::from_raw_fd(4)) })
}

#[cfg(windows)]
fn open_channel(handles: &[std::ffi::OsString]) -> io::Result<(File, File)> {
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType},
    };
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "private pipe missing");
    let [read, write] = handles else {
        return Err(invalid());
    };
    let handle = |argument: &std::ffi::OsString| {
        argument
            .to_str()
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|value| *value != 0 && *value != usize::MAX)
            .ok_or_else(invalid)
    };
    let (read, write) = (handle(read)?, handle(write)?);
    let valid = |raw: usize| unsafe { GetFileType(HANDLE(raw as _)) == FILE_TYPE_PIPE };
    if read == write || !valid(read) || !valid(write) {
        return Err(invalid());
    }
    // SAFETY: the private startup arguments are the two inherited pipe handles.
    Ok(unsafe {
        (
            File::from_raw_handle(read as _),
            File::from_raw_handle(write as _),
        )
    })
}
