//! Unix rendezvous and both directions of local kernel peer verification.

use super::{Endpoint, IncomingMcp, PAIR_BYTES, PAIR_TIMEOUT, decode_pair, denial, serve};
use devworks_adapters::mcp_inbound::unix::PrivateListener;
use devworks_core::protocol::Id;
use devworks_core::services::mcp_inbound::Binding;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const RENDEZVOUS: &str = "mcp-endpoint.json";
const MAX_ENDPOINT_BYTES: u64 = 4096;

fn owner() -> u32 {
    // SAFETY: geteuid takes no arguments and returns the effective UID.
    unsafe { libc::geteuid() }
}

fn endpoint(root: &Path) -> io::Result<Endpoint> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(RENDEZVOUS))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_ENDPOINT_BYTES
    {
        return Err(denial());
    }
    let mut bytes = Vec::new();
    file.take(MAX_ENDPOINT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENDPOINT_BYTES {
        return Err(denial());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

pub(super) async fn connect(root: &Path) -> io::Result<(Id, UnixStream)> {
    let target = endpoint(root)?;
    let path = &target.address;
    let directory = fs::symlink_metadata(path.parent().ok_or_else(denial)?)?;
    let socket = fs::symlink_metadata(path)?;
    if !directory.is_dir()
        || !socket.file_type().is_socket()
        || [directory, socket]
            .iter()
            .any(|entry| entry.uid() != owner() || entry.mode() & 0o077 != 0)
    {
        return Err(denial());
    }
    let stream = UnixStream::connect(path).await?;
    let peer = stream.peer_cred()?;
    if peer.uid() != owner()
        || peer.pid().and_then(|pid| u32::try_from(pid).ok()) != Some(target.pid)
    {
        return Err(denial());
    }
    let executable = process_executable(target.pid)?;
    let running = fs::metadata(executable)?;
    let installed = fs::metadata(std::env::current_exe()?)?;
    if (running.dev(), running.ino()) != (installed.dev(), installed.ino()) {
        return Err(denial());
    }
    Ok((target.instance, stream))
}

#[cfg(target_os = "macos")]
fn process_executable(pid: u32) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let pid = i32::try_from(pid).map_err(|_| denial())?;
    // SAFETY: The process ID was obtained from the live socket. The buffer is writable
    // for its full length, and proc_pidpath returns a bounded NUL-terminated path.
    let read = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if read <= 0 {
        return Err(io::Error::last_os_error());
    }
    let finish = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(denial)?;
    Ok(std::ffi::OsStr::from_bytes(&bytes[..finish]).into())
}

#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")) // dev-host: allow-path Linux kernel process name
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_executable(_: u32) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "desktop peer identity is unavailable",
    ))
}

pub(super) fn start(native: IncomingMcp, root: &Path, stop: CancellationToken) -> io::Result<()> {
    let nonce = native.ids.token("ipc");
    let instance = Id::new(&nonce).expect("host desktop instance");
    let dir = Directory(std::env::temp_dir().join(format!("dw-{}", &nonce[4..20])));
    let listener = PrivateListener::bind(&dir.0)?;
    let record = Endpoint {
        address: listener.path().to_owned(),
        pid: std::process::id(),
        instance,
    };
    let pending = root.join(format!(".mcp-{nonce}.tmp"));
    let path = root.join(RENDEZVOUS);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)?;
    let identity = file.metadata()?;
    let written = file
        .write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)
        .and_then(|_| fs::rename(&pending, &path));
    if let Err(error) = written {
        let _ = fs::remove_file(&pending);
        return Err(error);
    }
    let rendezvous = Rendezvous {
        path,
        device: identity.dev(),
        inode: identity.ino(),
    };
    tokio::spawn(accept(
        native,
        record.instance,
        listener,
        rendezvous,
        dir,
        stop,
    ));
    Ok(())
}

async fn accept(
    native: IncomingMcp,
    instance: Id,
    listener: PrivateListener,
    rendezvous: Rendezvous,
    directory: Directory,
    stop: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(8));
    loop {
        let handshake = tokio::select! {
            _ = stop.cancelled() => break,
            stream = listener.accept() => stream,
        };
        let Ok(mut connection) = handshake else {
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let id = instance.clone();
        let native = native.clone();
        let stop = stop.child_token();
        tokio::spawn(async move {
            let _slot = slot;
            let mut frame = [0; PAIR_BYTES];
            let got = tokio::select! {
                _ = stop.cancelled() => return,
                result = tokio::time::timeout(PAIR_TIMEOUT, connection.stream().read_exact(&mut frame)) => result,
            };
            let token = matches!(got, Ok(Ok(_)))
                .then(|| decode_pair(&frame, &id))
                .transpose();
            frame.fill(0);
            std::hint::black_box(&frame);
            let Ok(Some(token)) = token else { return };
            let credentials = native.credentials.clone();
            let authentication = tokio::task::spawn_blocking(move || {
                credentials.authenticate(Binding::LocalIpc, &token)
            });
            let client = tokio::select! {
                _ = stop.cancelled() => return,
                client = authentication => client,
            };
            if let Ok(Ok(client)) = client {
                let _ = serve(native, connection.into_stream(), client, stop).await;
            }
        });
    }
    drop(rendezvous);
    drop(listener);
    drop(directory);
}

struct Rendezvous {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}
