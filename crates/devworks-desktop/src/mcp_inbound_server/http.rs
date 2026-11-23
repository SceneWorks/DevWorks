//! Opt-in loopback Streamable HTTP. Each POST proves the current app registration before
//! handing a bounded request to the SDK, then rechecks the exact scope before delivering bytes.

use super::{IncomingMcp, REQUEST_LIMIT, RequestProof, Server};
use devworks_adapters::mcp_inbound::{MAX_REQUEST_BYTES, authenticate_http};
use devworks_core::ports::storage::ReadOptions;
use devworks_core::protocol::records::McpServerBinding;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{
    ALLOW, AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, WWW_AUTHENTICATE,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

type Sdk = StreamableHttpService<Server, LocalSessionManager>;
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

pub(super) async fn supervise(native: IncomingMcp) {
    let mut changes = native.runtime.state_changes();
    let mut bound = None;
    let mut recheck = tokio::time::interval(Duration::from_secs(2));
    loop {
        let credentials = native.credentials.clone();
        let desired = tokio::task::spawn_blocking(move || {
            let settings = credentials.settings(&ReadOptions::default()).ok()?;
            (settings.enabled
                && settings
                    .bindings
                    .0
                    .iter()
                    .any(|binding| matches!(binding, McpServerBinding::LoopbackHttp)))
            .then_some(settings.http_port)
            .flatten()
            .and_then(|port| u16::try_from(port.get()).ok())
        })
        .await
        .ok()
        .flatten();
        let actual = bound.as_ref().map(|(port, _)| *port);
        if actual != desired {
            if let Some((_, task)) = bound.take() {
                let task: CancellationToken = task;
                task.cancel();
            }
            let status = if let Some(port) = desired {
                match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                    Ok(listener) => {
                        let local = listener
                            .local_addr()
                            .expect("a bound loopback listener has an address");
                        let stop = native.stop.child_token();
                        tokio::spawn(listen(native.clone(), listener, stop.clone()));
                        bound = Some((port, stop));
                        Some(Ok(local))
                    }
                    Err(_) => Some(Err(
                        "DevWorks could not bind the configured loopback HTTP port.",
                    )),
                }
            } else {
                None
            };
            if let Ok(mut bindings) = native.bindings.lock() {
                bindings.http = status;
            }
        }
        tokio::select! {
            _ = native.stop.cancelled() => return,
            _ = recheck.tick() => (),
            _ = changes.changed() => (),
        }
    }
}

async fn listen(native: IncomingMcp, listener: TcpListener, stop: CancellationToken) {
    let Ok(local) = listener.local_addr() else {
        return;
    };
    let host = local.to_string();
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts([host])
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(MAX_REQUEST_BYTES as usize)
        .with_cancellation_token(stop.clone());
    let make = native.clone();
    let sdk = StreamableHttpService::new(
        move || Ok(Server(make.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let connections = Arc::new(Semaphore::new(16));
    loop {
        let accepted = tokio::select! {
            _ = stop.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else {
            tokio::time::sleep(Duration::from_millis(150)).await;
            continue;
        };
        let Ok(slot) = connections.clone().try_acquire_owned() else {
            continue;
        };
        let sdk = sdk.clone();
        let native = native.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let handler = service_fn(move |request| {
                let native = native.clone();
                let sdk = sdk.clone();
                async move { Ok::<_, Infallible>(handle(native, sdk, local, peer, request).await) }
            });
            let mut http = http1::Builder::new();
            http.keep_alive(false)
                .max_buf_size(16 * 1024)
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(5));
            tokio::select! {
                _ = stop.cancelled() => (),
                _ = tokio::time::timeout(REQUEST_LIMIT, http.serve_connection(TokioIo::new(stream), handler)) => (),
            }
        });
    }
}

async fn handle(
    native: IncomingMcp,
    sdk: Sdk,
    local: SocketAddr,
    peer: SocketAddr,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let (mut parts, body) = request.into_parts();
    if parts.method != Method::POST {
        let mut response = empty(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert(ALLOW, "POST".parse().unwrap());
        return response;
    }
    let credentials = native.credentials.clone();
    let authenticated = tokio::task::spawn_blocking(move || {
        let client = authenticate_http(&credentials, local, peer, &parts)?;
        parts.headers.remove(AUTHORIZATION);
        let proof = RequestProof::new(client);
        parts.extensions.insert(proof.clone());
        Ok::<_, devworks_core::services::mcp_inbound::CredentialError>((parts, proof))
    })
    .await;
    let Ok(Ok((parts, proof))) = authenticated else {
        let mut response = empty(StatusCode::UNAUTHORIZED);
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        return response;
    };
    if parts.headers.contains_key("mcp-session-id") || parts.headers.contains_key("last-event-id") {
        return empty(StatusCode::BAD_REQUEST);
    }
    if parts.headers.get_all("mcp-protocol-version").iter().count() > 1 {
        return empty(StatusCode::BAD_REQUEST);
    }
    let response = sdk.handle(Request::from_parts(parts, body)).await;
    let (mut parts, body) = response.into_parts();
    // The SDK's no-session JSON mode must never start an independent event stream.
    if parts
        .headers
        .get(CONTENT_TYPE)
        .is_some_and(|content| content.as_bytes().starts_with(b"text/event-stream"))
    {
        return empty(StatusCode::NOT_ACCEPTABLE);
    }
    let Ok(body) = Limited::new(body, RESPONSE_LIMIT).collect().await else {
        return empty(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let body = body.to_bytes();
    if !matches!(
        tokio::task::spawn_blocking(move || proof.check(&native)).await,
        Ok(true)
    ) {
        return empty(StatusCode::FORBIDDEN);
    }
    parts
        .headers
        .insert(CACHE_CONTROL, "no-store".parse().unwrap());
    parts.headers.insert(CONNECTION, "close".parse().unwrap());
    Response::from_parts(parts, Full::new(body))
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CACHE_CONTROL, "no-store")
        .header(CONNECTION, "close")
        .body(Full::default())
        .unwrap()
}
