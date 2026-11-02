//! Owned stdio helper experiment. Scripted state/authority, not production package host.
use crate::package::*;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fmt,
    io::{self, Read, Write},
};

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate object members")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element::<StrictValue>()? {
                    out.push(v.0);
                }
                Ok(StrictValue(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut out = Map::new();
                while let Some((k, v)) = a.next_entry::<String, StrictValue>()? {
                    if out.insert(k, v.0).is_some() {
                        return Err(serde::de::Error::custom("duplicate JSON member"));
                    }
                }
                Ok(StrictValue(Value::Object(out)))
            }
        }
        d.deserialize_any(V)
    }
}
pub fn strict_json(bytes: &[u8]) -> io::Result<Value> {
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let v = StrictValue::deserialize(&mut d).map_err(io::Error::other)?;
    d.end().map_err(io::Error::other)?;
    Ok(v.0)
}
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Value>> {
    let mut head = [0u8; 4];
    if r.read(&mut head[..1])? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut head[1..])?;
    let len = u32::from_le_bytes(head) as usize;
    if len == 0 || len > 1024 * 1024 {
        return Err(io::Error::other("frame length rejected before allocation"));
    }
    let mut data = vec![0; len];
    r.read_exact(&mut data)?;
    let value = strict_json(&data)?;
    if !value.is_object() {
        return Err(io::Error::other("one JSON-RPC object required"));
    }
    Ok(Some(value))
}
fn write_frame<W: Write>(w: &mut W, v: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    if bytes.len() > 1024 * 1024 {
        return Err(io::Error::other("output frame too large"));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}
fn error(id: Value, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32020,"message":message,"data":{"class":"ProtocolViolation"}}})
}
fn success(id: &str, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
struct Pending {
    host_id: String,
    invocation_id: String,
    operation_id: String,
    cancelled: bool,
    input: Value,
    token: String,
}
pub fn helper() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut r = stdin.lock();
    let mut w = stdout.lock();
    let mut session = String::new();
    let mut active = false;
    let mut registry = String::new();
    let mut state = String::new();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut credits: HashMap<String, (i64, u32)> = HashMap::new();
    while let Some(value) = read_frame(&mut r)? {
        if value["method"] == "broker.streamCredit" {
            let notification: PackageNotification =
                serde_json::from_value(value.clone()).map_err(io::Error::other)?;
            notification.validate().map_err(io::Error::other)?;
            if let PackageNotification::Credit { params, .. } = notification {
                let invocation = String::from(params.invocation_id.clone());
                let token = String::from(params.invocation_token.clone());
                if String::from(params.process_session_id.clone()) != session
                    || !pending
                        .values()
                        .any(|p| p.invocation_id == invocation && p.token == token && !p.cancelled)
                {
                    return Err(io::Error::other("credit for revoked or foreign invocation"));
                }
                let stream = format!("{}:{}", invocation, String::from(params.stream_id));
                let seq = String::from(params.sequence)
                    .parse::<i64>()
                    .map_err(io::Error::other)?;
                let old = credits.get(&stream).copied().unwrap_or((0, 0));
                if seq != old.0 + 1
                    || old
                        .1
                        .checked_add(params.grant_bytes)
                        .is_none_or(|n| n > 262144)
                {
                    return Err(io::Error::other("credit replay or window overflow"));
                }
                credits.insert(stream, (seq, old.1 + params.grant_bytes));
                write_frame(
                    &mut w,
                    &json!({"jsonrpc":"2.0","method":"package.progress","params":{"processSessionId":session,"invocationId":invocation,"invocationToken":token,"sequence":seq.to_string(),"text":"fixture credit received","droppedCount":"0"}}),
                )?;
            }
            continue;
        }
        if value.get("method").is_some() {
            let request: HostRequest =
                serde_json::from_value(value.clone()).map_err(io::Error::other)?;
            request.validate().map_err(io::Error::other)?;
            let id = value["id"].as_str().unwrap();
            if !seen.insert(id.to_owned()) {
                return Err(io::Error::other("duplicate host request ID"));
            }
            if !session.is_empty() && !id.starts_with(&(session.clone() + ":h:")) {
                return Err(io::Error::other("wrong request ID namespace"));
            }
            match request {
                HostRequest::Initialize { params, .. } => {
                    let offered: String = params.process_session_id.into();
                    if !session.is_empty() || !id.starts_with(&(offered.clone() + ":h:")) {
                        return Err(io::Error::other("initialization identity mismatch"));
                    }
                    let supported = ["cancel".to_owned(), "credits".to_owned()];
                    if params
                        .required_features
                        .iter()
                        .any(|f| !supported.contains(&String::from(f.clone())))
                    {
                        write_frame(&mut w, &error(json!(id), "unsupported required feature"))?;
                        continue;
                    }
                    session = offered;
                    write_frame(
                        &mut w,
                        &success(
                            id,
                            json!({"kind":"Initialized","selected":{"major":1,"minor":0},"manifestDigest":params.expected_manifest_digest,"contributionIds":["fixture/tool"],"features":supported}),
                        ),
                    )?;
                }
                HostRequest::Activate { params, .. } => {
                    if session != String::from(params.process_session_id)
                        || !id.starts_with(&(session.clone() + ":h:"))
                    {
                        write_frame(&mut w, &error(json!(id), "session mismatch"))?;
                        continue;
                    }
                    if !pending.is_empty() {
                        write_frame(&mut w, &error(json!(id), "activation requires quiescence"))?;
                        continue;
                    }
                    registry = String::from(params.registry_revision);
                    state = String::from(params.state_generation);
                    active = true;
                    write_frame(
                        &mut w,
                        &success(
                            id,
                            json!({"kind":"Activated","registryRevision":registry,"stateGeneration":state}),
                        ),
                    )?;
                }
                HostRequest::Invoke { params, .. } => {
                    if !active
                        || session != String::from(params.process_session_id.clone())
                        || String::from(params.registry_revision.clone()) != registry
                        || String::from(params.state_generation.clone()) != state
                        || !id.starts_with(&(session.clone() + ":h:"))
                    {
                        write_frame(
                            &mut w,
                            &error(json!(id), "invocation is not in active generation"),
                        )?;
                        continue;
                    }
                    let invocation = String::from(params.invocation_id.clone());
                    if pending.values().any(|p| p.invocation_id == invocation) {
                        return Err(io::Error::other("duplicate invocation"));
                    }
                    let callback = format!("{session}:p:{invocation}");
                    let input = serde_json::to_value(&params.input)?;
                    pending.insert(
                        callback.clone(),
                        Pending {
                            host_id: id.to_owned(),
                            invocation_id: invocation.clone(),
                            operation_id: String::from(params.operation_id.0),
                            cancelled: false,
                            input: input.clone(),
                            token: String::from(params.invocation_token.clone()),
                        },
                    );
                    write_frame(
                        &mut w,
                        &json!({"jsonrpc":"2.0","id":callback,"method":"broker.call","params":{"processSessionId":session,"invocationId":invocation,"invocationToken":params.invocation_token,"actionKey":"read-input","call":{"kind":"ArtifactRead","artifact":input}}}),
                    )?;
                }
                HostRequest::Cancel { params, .. } => {
                    if session != String::from(params.process_session_id) {
                        return Err(io::Error::other("cancel session mismatch"));
                    }
                    let invocation = String::from(params.invocation_id);
                    for p in pending
                        .values_mut()
                        .filter(|p| p.invocation_id == invocation)
                    {
                        p.cancelled = true;
                    }
                    write_frame(
                        &mut w,
                        &success(
                            id,
                            json!({"kind":"CancelAcknowledged","invocationId":invocation}),
                        ),
                    )?;
                }
                HostRequest::Shutdown { params, .. } => {
                    if session != String::from(params.process_session_id) {
                        return Err(io::Error::other("shutdown session mismatch"));
                    }
                    write_frame(&mut w, &success(id, json!({"kind":"ShutdownAcknowledged"})))?;
                    break;
                }
            }
        } else {
            let response: RpcResponse =
                serde_json::from_value(value.clone()).map_err(io::Error::other)?;
            response.validate().map_err(io::Error::other)?;
            let id = value["id"]
                .as_str()
                .ok_or_else(|| io::Error::other("unsolicited parse error"))?;
            let p = pending
                .remove(id)
                .ok_or_else(|| io::Error::other("unknown or duplicate terminal response"))?;
            let outcome = if p.cancelled {
                json!({"kind":"OutcomeUnknown","operationId":p.operation_id})
            } else {
                match response {
                    RpcResponse::Success(s)
                        if matches!(s.result, RpcResult::BrokerCompleted { .. }) =>
                    {
                        json!({"kind":"Completed","artifacts":[p.input]})
                    }
                    _ => json!({"kind":"Failed","code":"broker-failed"}),
                }
            };
            write_frame(
                &mut w,
                &success(
                    &p.host_id,
                    json!({"kind":"InvocationFinished","invocationId":p.invocation_id,"outcome":outcome}),
                ),
            )?;
        }
    }
    Ok(())
}
