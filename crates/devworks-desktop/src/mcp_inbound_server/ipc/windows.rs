//! Same-user Windows pipe rendezvous. The bridge verifies the installed desktop process before
//! presenting its credential, and the desktop checks the peer of the credential frame.

use super::{IncomingMcp, PAIR_BYTES, PAIR_TIMEOUT, decode_pair, denial, serve};
use devworks_adapters::mcp_inbound::windows::{PrivatePipe, open_desktop};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::protocol::Id;
use devworks_core::services::mcp_inbound::Binding;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::NamedPipeClient;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const INSTANCE_BYTES: usize = 28;

fn root_id(root: &Path) -> io::Result<Id> {
    let root = std::fs::canonicalize(root)?;
    let identity: Vec<u8> = root
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    Ok(Id::new(sha256_hex(&identity)).expect("hex identifier"))
}

pub(super) async fn connect(root: &Path) -> io::Result<(Id, NamedPipeClient)> {
    let (mut pipe, _) = open_desktop(&root_id(root)?)?;
    let mut challenge = [0; INSTANCE_BYTES];
    pipe.read_exact(&mut challenge).await?;
    let instance = std::str::from_utf8(&challenge).map_err(|_| denial())?;
    if !instance.starts_with("ipc-") {
        return Err(denial());
    }
    Ok((Id::new(instance).map_err(|_| denial())?, pipe))
}

pub(super) fn start(native: IncomingMcp, root: &Path, stop: CancellationToken) -> io::Result<()> {
    let listener = PrivatePipe::bind(&root_id(root)?)?;
    let instance = Id::new(native.ids.token("ipc")).expect("host desktop instance");
    tokio::spawn(accept(native, instance, listener, stop));
    Ok(())
}

async fn accept(
    native: IncomingMcp,
    instance: Id,
    mut listener: PrivatePipe,
    stop: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(8));
    loop {
        let connected = tokio::select! {
            _ = stop.cancelled() => break,
            connected = listener.connect() => connected,
        };
        if connected.is_err() {
            break;
        }
        let Ok(next) = listener.next() else { break };
        let mut connection = std::mem::replace(&mut listener, next);
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
                result = tokio::time::timeout(PAIR_TIMEOUT, async {
                    connection.stream().write_all(id.as_str().as_bytes()).await
                        .map_err(|_| denial())?;
                    connection.read_peer_frame(&mut frame).await.map_err(|_| denial())
                }) => result,
            };
            let token = matches!(got, Ok(Ok(())))
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
}
