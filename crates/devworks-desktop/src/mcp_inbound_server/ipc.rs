//! Authenticated private desktop IPC for the installed stdio bridge.

use super::{IncomingMcp, RequestProof, Server, denied};
use devworks_adapters::mcp_inbound::MAX_REQUEST_BYTES;
use devworks_core::ports::secrets::Secret;
use devworks_core::ports::storage::ReadOptions;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::McpServerBinding;
use devworks_core::services::mcp_inbound::{AuthenticatedClient, CredentialError};
use futures_util::StreamExt;
use rmcp::ServiceExt;
use rmcp::model::{
    ClientJsonRpcMessage, ErrorCode, ErrorData, GetExtensions, JsonRpcMessage, RequestId,
    ServerJsonRpcMessage,
};
use rmcp::service::RoleServer;
use rmcp::transport::{Transport, io::stdio_pipes};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

const PAIR_BYTES: usize = 256;
const PAIR_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IPC_REQUESTS: usize = 32;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(unix)]
struct Endpoint {
    address: PathBuf,
    pid: u32,
    instance: Id,
}

fn pairing(instance: &Id, token: &Secret) -> io::Result<[u8; PAIR_BYTES]> {
    let instance = instance.as_str().as_bytes();
    let secret = token.expose().as_bytes();
    if instance.len() + secret.len() + 2 > PAIR_BYTES || secret.contains(&0) {
        return Err(denial());
    }
    let mut frame = [0; PAIR_BYTES];
    frame[..instance.len()].copy_from_slice(instance);
    frame[instance.len() + 1..instance.len() + 1 + secret.len()].copy_from_slice(secret);
    Ok(frame)
}

fn decode_pair(frame: &[u8], instance: &Id) -> Result<Secret, CredentialError> {
    let mut members = frame.splitn(3, |byte| *byte == 0);
    let instance_bytes = members.next().ok_or(CredentialError::Denied)?;
    let token = members.next().ok_or(CredentialError::Denied)?;
    let padding = members.next().ok_or(CredentialError::Denied)?;
    if instance_bytes != instance.as_str().as_bytes() || padding.iter().any(|byte| *byte != 0) {
        return Err(CredentialError::Denied);
    }
    let token = std::str::from_utf8(token).map_err(|_| CredentialError::Denied)?;
    Ok(Secret::new(token))
}

fn denial() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "DevWorks desktop pairing denied",
    )
}

pub(super) async fn supervise(native: IncomingMcp, root: PathBuf) {
    let mut changes = native.runtime.state_changes();
    let mut bound: Option<CancellationToken> = None;
    let mut recheck = tokio::time::interval(Duration::from_secs(2));
    loop {
        let credentials = native.credentials.clone();
        let enabled = tokio::task::spawn_blocking(move || {
            credentials
                .settings(&ReadOptions::default())
                .is_ok_and(|settings| {
                    settings.enabled
                        && settings
                            .bindings
                            .0
                            .iter()
                            .any(|binding| matches!(binding, McpServerBinding::Stdio))
                })
        })
        .await
        .unwrap_or(false);
        if enabled && bound.is_none() {
            let stop = native.stop.child_token();
            let status = platform::start(native.clone(), &root, stop.clone())
                .map_err(|_| "DevWorks could not start its private stdio endpoint.");
            if status.is_ok() {
                bound = Some(stop);
            }
            if let Ok(mut bindings) = native.bindings.lock() {
                bindings.stdio = Some(status);
            }
        } else if !enabled && let Some(stop) = bound.take() {
            stop.cancel();
            if let Ok(mut bindings) = native.bindings.lock() {
                bindings.stdio = None;
            }
        }
        tokio::select! {
            _ = native.stop.cancelled() => return,
            _ = recheck.tick() => (),
            _ = changes.changed() => (),
        }
    }
}

pub fn run(root: &Path) -> io::Result<()> {
    let token = Secret::new(std::env::var("DEVWORKS_MCP_TOKEN").map_err(|_| denial())?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let (instance, mut io) = tokio::time::timeout(PAIR_TIMEOUT, platform::connect(root))
            .await
            .map_err(io::Error::other)??;
        let mut frame = pairing(&instance, &token)?;
        let sent = io.write_all(&frame).await;
        frame.fill(0);
        std::hint::black_box(&frame);
        sent?;
        let mut accepted = [0_u8];
        tokio::time::timeout(PAIR_TIMEOUT, io.read_exact(&mut accepted))
            .await
            .map_err(io::Error::other)??;
        if accepted != [1] {
            return Err(denial());
        }
        let (mut input, mut output) = stdio_pipes();
        let (mut from_desktop, mut to_desktop) = tokio::io::split(io);
        let send = async {
            tokio::io::copy(&mut input, &mut to_desktop).await?;
            to_desktop.shutdown().await
        };
        let receive = async {
            tokio::io::copy(&mut from_desktop, &mut output).await?;
            output.flush().await
        };
        tokio::pin!(receive);
        tokio::select! {
            result = &mut receive => result,
            sent = send => { sent?; receive.await }
        }
    });
    // On platforms with blocking stdio, the read cannot be interrupted. The bridge is a
    // short-lived CLI process, so exiting closes that handle after the desktop disconnects.
    runtime.shutdown_background();
    result
}

async fn serve<T>(
    native: IncomingMcp,
    mut stream: T,
    client: AuthenticatedClient,
    stop: CancellationToken,
) -> io::Result<()>
where
    T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    stream.write_all(&[1]).await?;
    let (read, write) = tokio::io::split(stream);
    let transport = Checked {
        read: FramedRead::new(
            read,
            LinesCodec::new_with_max_length(MAX_REQUEST_BYTES as usize),
        ),
        write: Arc::new(tokio::sync::Mutex::new(write)),
        native: native.clone(),
        client: Arc::new(client),
        pending: Arc::default(),
    };
    let started = tokio::time::timeout(
        PAIR_TIMEOUT,
        Server(native).serve_with_ct(transport, stop.clone()),
    )
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    let _ = started.waiting().await;
    Ok(())
}

struct Checked<T: AsyncRead + AsyncWrite> {
    read: FramedRead<ReadHalf<T>, LinesCodec>,
    write: Arc<tokio::sync::Mutex<WriteHalf<T>>>,
    native: IncomingMcp,
    client: Arc<AuthenticatedClient>,
    pending: Arc<Mutex<HashMap<RequestId, RequestProof>>>,
}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Transport<RoleServer> for Checked<T> {
    type Error = io::Error;

    fn send(
        &mut self,
        item: ServerJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let id = match &item {
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        let proof = id
            .as_ref()
            .and_then(|id| self.pending.lock().ok()?.remove(id));
        let native = self.native.clone();
        let write = self.write.clone();
        async move {
            let mut bytes = serde_json::to_vec(&item).map_err(io::Error::other)?;
            let mut wire = write.lock().await;
            let allowed = match proof {
                Some(proof) => matches!(
                    tokio::task::spawn_blocking(move || proof.check(&native)).await,
                    Ok(true)
                ),
                None => true,
            };
            if !allowed || bytes.len() > 2 * MAX_REQUEST_BYTES as usize {
                bytes = serde_json::to_vec(&ServerJsonRpcMessage::error(denied(), id))
                    .map_err(io::Error::other)?;
            }
            bytes.push(b'\n');
            tokio::time::timeout(super::REQUEST_LIMIT, async {
                wire.write_all(&bytes).await?;
                wire.flush().await
            })
            .await
            .map_err(io::Error::other)?
        }
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let mut message = loop {
            let line = self.read.next().await?.ok()?;
            match serde_json::from_str::<ClientJsonRpcMessage>(&line) {
                Ok(message) => break message,
                Err(_) => {
                    self.send(ServerJsonRpcMessage::error(
                        ErrorData::new(ErrorCode::PARSE_ERROR, "Invalid MCP message", None),
                        None,
                    ))
                    .await
                    .ok()?;
                }
            }
        };
        if let JsonRpcMessage::Request(request) = &mut message {
            let proof = RequestProof {
                client: self.client.clone(),
                delivery: Arc::default(),
            };
            let mut pending = self.pending.lock().ok()?;
            if pending.len() >= MAX_IPC_REQUESTS || pending.contains_key(&request.id) {
                return None;
            }
            pending.insert(request.id.clone(), proof.clone());
            request.request.extensions_mut().insert(proof);
        }
        Some(message)
    }

    async fn close(&mut self) -> io::Result<()> {
        self.write.lock().await.shutdown().await
    }
}
