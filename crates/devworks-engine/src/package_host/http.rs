//! Brokered HTTP: a package's `broker.call` of kind `Http` through a named host connection.
//!
//! * **Connections** are the workspace setting `packages.connections`: each has a name (the
//!   call's `connectionHandle`), the network endpoint it serves (the call's `endpointId`, which
//!   the approved manifest must grant as `network:<endpoint>`), an `https://` base URL and
//!   optionally a credential, a secret-store reference resolved only here, when the request is
//!   sent, and sent only in its header (default `Authorization`) to that base URL. The package
//!   names a connection; it never sees its URL's credential, its secret or any header the host
//!   adds.
//! * **Targets**: the call's relative path is appended to the base URL; the result must keep the
//!   base's origin and stay under its path, so a path cannot escape the connection.
//! * **Egress** is `web.fetch`'s own ([`egress::admit_destination`], [`egress::send`]): the host
//!   resolves once, every address must be allowed (`web.fetchAllowPrivateNetwork` for loopback
//!   and private ranges; link-local and metadata never), and the request goes only to a checked
//!   address. No redirect is followed, so the credential never reaches another origin; a 3xx
//!   answer is returned to the package as it is. The whole request ends within `web.timeoutMs`.
//! * **Bodies**: a request body is an artifact of the invocation (at most [`MAX_BODY`] bytes);
//!   the answer (status, headers without cookies, body of at most [`MAX_BODY`] bytes, with any
//!   occurrence of the credential replaced) is published as an artifact the package receives.
//! * **Attribution**: each call is journaled before it is sent as a `package.broker.http`
//!   operation naming the invocation (its durable operation and invocation ID), the connection,
//!   endpoint, method and path, with a history event attributed to the package's actor; its
//!   outcome records the status and the bytes sent and received. A retried call under the same
//!   action key returns its recorded answer, and one whose outcome is unknown is never sent
//!   again.
use super::broker::{self, Journal};
use super::session::{Slot, rpc_error};
use super::{PackageHost, SlotContext};
use devworks_core::fingerprint::{canonical_digest, sha256_hex};
use devworks_core::ports::secrets::{SecretRef, SecretResolver};
use devworks_core::ports::storage::{
    ConfigScope, ConfigurationRead, OperationRecord, ReadOptions, StorageError, WriteOptions,
    read_snapshot, run_write,
};
use devworks_core::ports::web::{HopTransport, NetFault, Resolver};
use devworks_core::protocol::provider::ArtifactRef as HostArtifact;
use devworks_core::protocol::{Id, OperationId, OperationState};
use devworks_core::services::web::{USER_AGENT, egress, read_policy};
use devworks_core::settings::catalog::{PACKAGE_CONNECTIONS_KEY, descriptor};
use devworks_core::settings::model::{chain, current_layer};
use devworks_extension_protocol::package::{
    BrokerCall, BrokerInvoke, HttpMethod, RpcError, RpcErrorClass, RpcResult,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The journal capability of one brokered HTTP call.
pub const HTTP_CAPABILITY: &str = "package.broker.http";
/// Largest request or response body.
pub const MAX_BODY: usize = 1024 * 1024;

/// The network ports brokered HTTP calls use (the web tools' resolver and transport and the
/// secret store settings references resolve through), attached once by the host's composer.
#[derive(Clone)]
pub struct NetworkPorts {
    pub resolver: Arc<dyn Resolver>,
    pub transport: Arc<dyn HopTransport>,
    pub secrets: Arc<dyn SecretResolver>,
}

/// One configured connection.
#[derive(Clone, Debug)]
struct Connection {
    name: String,
    endpoint_id: String,
    base: egress::Url,
    credential: Option<(SecretRef, String)>,
}

/// The configured connections (a stored value under another schema fails closed).
fn connections(read: &dyn ConfigurationRead) -> Result<Vec<Connection>, StorageError> {
    let d = descriptor(PACKAGE_CONNECTIONS_KEY).expect("catalog key");
    let layer = current_layer(read, &ConfigScope::Workspace)?;
    let value = chain(d, std::slice::from_ref(&layer))
        .map_err(|e| StorageError::IncompatibleSchema(format!("{e:?}")))?
        .into_iter()
        .next()
        .map(|e| e.value.value.0)
        .unwrap_or_else(|| d.default_value());
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Connection {
                name: c["name"].as_str()?.to_owned(),
                endpoint_id: c["endpointId"].as_str()?.to_owned(),
                base: egress::parse(c["baseUrl"].as_str()?).ok()?,
                credential: match c.get("credential") {
                    None => None,
                    Some(r) => Some((
                        SecretRef::from_setting(r)?,
                        c["credentialHeader"]
                            .as_str()
                            .unwrap_or("Authorization")
                            .to_owned(),
                    )),
                },
            })
        })
        .collect())
}

/// `relative` under `base`, or `None` when it would leave the base's origin or path.
fn target(base: &egress::Url, relative: &str) -> Option<egress::Url> {
    let url = egress::parse(&format!(
        "{}{relative}",
        base.as_str().trim_end_matches('/')
    ))
    .ok()?;
    let root = base.path().trim_end_matches('/');
    let under = url.path() == root || url.path().starts_with(&format!("{root}/"));
    (url.origin() == base.origin() && under).then_some(url)
}

fn method_name(m: &HttpMethod) -> &'static str {
    match m {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
    }
}

fn denied(message: &str) -> RpcError {
    rpc_error(RpcErrorClass::PermissionDenied, message)
}

/// Replaces every occurrence of `secret` in `bytes`.
fn scrub(bytes: &mut Vec<u8>, secret: &[u8]) {
    if secret.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(secret) {
            out.extend_from_slice(b"[redacted]");
            i += secret.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    *bytes = out;
}

/// The recorded answer of a call journaled before under the same identity.
fn recorded(op: &OperationRecord) -> Option<Value> {
    op.attempts
        .iter()
        .flat_map(|a| a.observations.iter())
        .find_map(|o| {
            o.get("result")
                .or_else(|| o.as_object()?.values().find_map(|v| v.get("result")))
                .cloned()
        })
}

/// Answers one `broker.call` of kind `Http` whose endpoint the approved manifest grants.
pub(super) fn call(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    params: &BrokerInvoke,
) -> Result<RpcResult, RpcError> {
    let BrokerCall::Http {
        connection_handle,
        endpoint_id,
        http_method,
        relative_path,
        body,
    } = &params.call
    else {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "not an HTTP call",
        ));
    };
    let (Some(network), Some(ports)) = (host.network.get(), host.broker.get()) else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "brokered HTTP calls are not composed in this host",
        ));
    };
    let read = read_snapshot(&*host.store, &ReadOptions::default(), |s| {
        Ok((
            connections(s.configuration())?,
            read_policy(s.configuration())?,
        ))
    });
    let (all, policy) = match read {
        Ok(r) => r,
        Err(e) => {
            return Err(broker::host_failure(
                host,
                &ctx.package_id,
                RpcErrorClass::Unavailable,
                "the host connections could not be read",
                &e,
            ));
        }
    };
    let Some(connection) = all
        .into_iter()
        .find(|c| c.name == connection_handle.as_str() && c.endpoint_id == endpoint_id.as_str())
    else {
        return Err(denied(
            "no host connection of this name serves this endpoint",
        ));
    };
    let Some(url) = target(&connection.base, relative_path) else {
        return Err(denied("the path leaves the connection's base URL"));
    };
    let method = method_name(http_method);
    // The request body: an artifact of this invocation, read as the host.
    let (content, media_type) = match body {
        None => (Vec::new(), None),
        Some(artifact) => {
            if !ctx.in_scope(artifact) {
                return Err(denied("the body is not an artifact of this invocation"));
            }
            let reference: HostArtifact = serde_json::to_value(artifact)
                .and_then(serde_json::from_value)
                .map_err(|_| {
                    rpc_error(RpcErrorClass::ProtocolViolation, "invalid body artifact")
                })?;
            let bytes = ports
                .artifacts
                .read_exact(&*host.store, &host.host_ctx(), &reference, MAX_BODY as u64)
                .map_err(|_| denied("the body artifact is not readable"))?
                .0;
            (bytes, Some(artifact.media_type.as_str().to_owned()))
        }
    };
    let action = params.action_key.as_str();
    let identity = sha256_hex(format!("{}\0{action}", ctx.operation_id).as_bytes());
    let operation = OperationId(Id::new(format!("pkgcall-{identity}")).expect("bounded id"));
    let digest = canonical_digest(&json!({"connection": connection.name,
        "endpointId": connection.endpoint_id, "method": method, "relativePath": relative_path,
        "body": sha256_hex(&content)}))
    .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid call"))?;
    let journaled = broker::journal(
        host,
        ctx,
        slot,
        &Journal {
            operation: &operation,
            capability: HTTP_CAPABILITY,
            action,
            name: &connection.endpoint_id,
            digest: &digest,
            // The path without its query: a query may carry what the package put there.
            detail: json!({"connection": connection.name, "endpointId": connection.endpoint_id,
                "method": method, "path": url.path()}),
        },
    )?;
    if let Some(earlier) = journaled {
        // A retried call: its recorded answer, never a second request.
        return match (earlier.state, recorded(&earlier)) {
            (OperationState::ObservedComplete, Some(result)) => {
                let artifact: wire::ArtifactRef = serde_json::from_value(result).map_err(|_| {
                    rpc_error(RpcErrorClass::OutcomeUnknown, "invalid recorded answer")
                })?;
                ctx.admit_artifact(&artifact);
                Ok(RpcResult::BrokerCompleted { artifact })
            }
            (OperationState::Failed, _) => Err(rpc_error(
                RpcErrorClass::Conflict,
                "this call already failed under this action key",
            )),
            _ => Err(outcome_unknown(&operation)),
        };
    }
    let finish = |state: OperationState, observation: Value| {
        let operation = operation.clone();
        let _ = run_write(&*host.store, WriteOptions::default(), move |tx| {
            broker::settle(tx, &operation, state, observation)
        });
    };
    let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
    let cancel = vec![slot.cancellation.clone()];
    let port = egress::port(&url);
    let addrs = match egress::admit_destination(
        &*network.resolver,
        &url,
        port,
        policy.allow_private,
        deadline,
        &cancel,
    ) {
        egress::Admission::Allowed { addrs, .. } => addrs,
        egress::Admission::Blocked { class, listed } => {
            finish(
                OperationState::Failed,
                json!({"refused": class.as_str(), "addresses": listed.iter().map(|(a, c)| json!({"address": a, "class": c})).collect::<Vec<_>>()}),
            );
            return Err(denied(&format!(
                "the connection's address is refused ({})",
                class.as_str()
            )));
        }
        egress::Admission::NoAddresses => {
            finish(OperationState::Failed, json!({"failed": "no addresses"}));
            return Err(rpc_error(
                RpcErrorClass::Unavailable,
                "the connection's host has no address",
            ));
        }
        egress::Admission::Unresolved(f) => {
            finish(OperationState::Failed, json!({"failed": f.reason()}));
            return Err(rpc_error(
                RpcErrorClass::Unavailable,
                "the connection's host could not be resolved",
            ));
        }
    };
    let secret = match &connection.credential {
        None => None,
        Some((reference, header)) => match network.secrets.resolve(reference) {
            Ok(secret) => Some((header.clone(), secret)),
            Err(why) => {
                finish(
                    OperationState::Failed,
                    json!({"failed": format!("credential {why:?}")}),
                );
                return Err(rpc_error(
                    RpcErrorClass::Unavailable,
                    "the connection's credential is unavailable",
                ));
            }
        },
    };
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some((header, value)) = &secret {
        headers.push((header.as_str(), value.expose()));
    }
    if let Some(media_type) = &media_type {
        headers.push(("Content-Type", media_type.as_str()));
    }
    let sent = egress::send(
        &*network.transport,
        &url,
        port,
        &addrs,
        &egress::Outgoing {
            method,
            body: &content,
            deadline,
            cancel,
            user_agent: USER_AGENT,
            accept: "*/*",
            headers: &headers,
        },
        &mut |_, _| {},
    );
    let response = match sent {
        Ok(r) => r,
        Err(f) => return Err(failed_send(&f, &operation, &finish)),
    };
    let status = response.status;
    let mut kept: Vec<(String, String)> = response
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("set-cookie"))
        .cloned()
        .collect();
    let mut answer = Vec::new();
    let read = response
        .body
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut answer);
    if let Err(e) = read {
        let f = NetFault::AfterSend {
            reason: e.to_string(),
            timed_out: e.kind() == std::io::ErrorKind::TimedOut,
        };
        return Err(failed_send(&f, &operation, &finish));
    }
    let truncated = answer.len() > MAX_BODY;
    answer.truncate(MAX_BODY);
    let received = answer.len();
    if let Some((_, value)) = &secret {
        scrub(&mut answer, value.expose().as_bytes());
        for (_, v) in &mut kept {
            let mut bytes = std::mem::take(v).into_bytes();
            scrub(&mut bytes, value.expose().as_bytes());
            *v = String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    let envelope = json!({"status": status,
        "headers": kept.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "bodyBase64Url": devworks_core::ports::http::base64url(&answer), "truncated": truncated});
    let owner = ctx
        .grants
        .principal()
        .cloned()
        .unwrap_or_else(|| host.host_ctx());
    let produced = broker::publish(
        host,
        &ports.artifacts,
        &owner,
        &operation,
        None,
        envelope.to_string().as_bytes(),
    )
    .map_err(|e| {
        finish(
            OperationState::OutcomeUnknown,
            json!({"status": status, "failed": "the answer was not published"}),
        );
        broker::host_failure(
            host,
            &ctx.package_id,
            RpcErrorClass::OutcomeUnknown,
            "the call's answer could not be published",
            &e,
        )
    })?;
    finish(
        OperationState::ObservedComplete,
        json!({"result": produced, "status": status, "requestBytes": content.len(),
            "responseBytes": received, "truncated": truncated}),
    );
    let produced: wire::ArtifactRef = serde_json::to_value(&produced)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::OutcomeUnknown, "invalid result artifact"))?;
    ctx.admit_artifact(&produced);
    Ok(RpcResult::BrokerCompleted { artifact: produced })
}

fn outcome_unknown(operation: &OperationId) -> RpcError {
    let mut e = rpc_error(
        RpcErrorClass::OutcomeUnknown,
        "the call may have reached the server; it is not sent again",
    );
    e.data.operation_id = wire::Id::try_from(operation.0.as_str().to_owned())
        .ok()
        .map(wire::OperationId);
    e
}

/// The package's error for a request that produced no complete answer, recording it.
fn failed_send(
    f: &NetFault,
    operation: &OperationId,
    finish: &dyn Fn(OperationState, Value),
) -> RpcError {
    if f.dispatched() {
        finish(
            OperationState::OutcomeUnknown,
            json!({"failed": f.reason(), "sent": true}),
        );
        outcome_unknown(operation)
    } else {
        finish(
            OperationState::Failed,
            json!({"failed": f.reason(), "sent": false}),
        );
        rpc_error(RpcErrorClass::Unavailable, "the request could not be sent")
    }
}
