//! The journaled and queued payloads of MCP effects, and the history events they record. Each
//! shape is one serde type: the request side serializes it once and the dispatching side parses
//! it once, so a renamed or retyped field fails to compile instead of failing at dispatch. The
//! JSON member names are stored data and must not change.
use crate::context::PrincipalKind;
use crate::ports::AuthorityFence;
use crate::ports::storage::ConfigScope;
use crate::protocol::provider::{SchemaRef, ToolCallKey};
use crate::protocol::{ActorId, Id, ProjectId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A scope stored by its key.
mod scope_key {
    use super::ConfigScope;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(scope: &ConfigScope, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&scope.key())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ConfigScope, D::Error> {
        let key = String::deserialize(d)?;
        ConfigScope::from_key(&key).ok_or_else(|| D::Error::custom("unknown MCP scope"))
    }
}
/// A principal kind stored by its persisted name.
mod kind_name {
    use super::PrincipalKind;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};
    pub fn serialize<S: Serializer>(kind: &PrincipalKind, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(kind.as_str())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PrincipalKind, D::Error> {
        let name = String::deserialize(d)?;
        PrincipalKind::parse(&name).ok_or_else(|| D::Error::custom("unknown principal kind"))
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("an MCP effect payload serializes")
}

/// Journal payload of `mcp.connect` and `mcp.discover`. Only discovery names a catalog revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConnectEffect {
    #[serde(with = "scope_key")]
    pub scope: ConfigScope,
    pub server_id: Id,
    pub pending_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<i64>,
    pub principal: ActorId,
    #[serde(with = "kind_name")]
    pub principal_kind: PrincipalKind,
    #[serde(default)]
    pub session_id: Option<Id>,
    pub fences: Vec<AuthorityFence>,
}
impl ConnectEffect {
    pub fn journal(&self) -> Value {
        to_value(self)
    }
    pub fn parse(payload: &Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(payload)
    }
}

/// Journal payload of `mcp.disable` and `mcp.remove`: the server as it was saved.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DisconnectEffect {
    #[serde(with = "scope_key")]
    pub scope: ConfigScope,
    pub server_id: Id,
    pub server: Value,
    pub principal: ActorId,
    pub fences: Vec<AuthorityFence>,
}
impl DisconnectEffect {
    pub fn journal(&self) -> Value {
        to_value(self)
    }
    pub fn parse(payload: &Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(payload)
    }
}

/// Who made a tool call and under which fences: the members of an `mcp.tool` journal payload
/// that sampling authorization also reads.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolCaller {
    #[serde(with = "scope_key")]
    pub scope: ConfigScope,
    pub server_id: Id,
    pub catalog_revision: i64,
    pub principal: ActorId,
    #[serde(with = "kind_name")]
    pub principal_kind: PrincipalKind,
    pub session_id: Id,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    pub fences: Vec<AuthorityFence>,
    pub call: ToolCallKey,
}
impl ToolCaller {
    pub fn parse(payload: &Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(payload)
    }
}

/// Journal payload of `mcp.tool`: the exact call the agent was authorized to make.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolEffect {
    pub server_revision: i64,
    pub capability_id: Id,
    pub schema: SchemaRef,
    pub wire_name: String,
    pub arguments: Value,
    #[serde(flatten)]
    pub caller: ToolCaller,
}
impl ToolEffect {
    pub fn journal(&self) -> Value {
        to_value(self)
    }
    /// Refuses a payload whose arguments are not an object.
    pub fn parse(payload: &Value) -> Result<Self, serde_json::Error> {
        use serde::de::Error;
        let effect = Self::deserialize(payload)?;
        if !effect.arguments.is_object() {
            return Err(serde_json::Error::custom("MCP tool arguments"));
        }
        Ok(effect)
    }
}

/// Queue payload of every MCP effect: the server it concerns and, for a tool, the call.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct QueuedEffect<'a> {
    pub server_id: &'a Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<&'a ToolCallKey>,
}
impl QueuedEffect<'_> {
    pub fn payload(&self) -> Value {
        to_value(self)
    }
}

/// History event payload of a server change (`scope` is the saved scope's key).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServerEvent<'a> {
    pub server_id: &'a Id,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<i64>,
}
impl<'a> ServerEvent<'a> {
    pub fn new(server_id: &'a Id, scope: &ConfigScope) -> Self {
        Self {
            server_id,
            scope: scope.key(),
            catalog_revision: None,
        }
    }
    pub fn catalog(mut self, revision: i64) -> Self {
        self.catalog_revision = Some(revision);
        self
    }
    pub fn payload(&self) -> Value {
        to_value(self)
    }
}

/// History event payload of a tool request.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolRequestedEvent<'a> {
    pub server_id: &'a Id,
    pub capability_id: &'a Id,
    pub call: &'a ToolCallKey,
}
impl ToolRequestedEvent<'_> {
    pub fn payload(&self) -> Value {
        to_value(self)
    }
}

/// History event payload of a sampling decision.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SamplingEvent<'a> {
    pub server_id: &'a Id,
    pub attempt_id: &'a Id,
    pub ordinal: u8,
    pub reason: Option<&'a str>,
}
impl SamplingEvent<'_> {
    pub fn payload(&self) -> Value {
        to_value(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn id(value: &str) -> Id {
        Id::new(value).unwrap()
    }
    fn policy_fence() -> AuthorityFence {
        AuthorityFence::Policy {
            project_id: Some(ProjectId(id("project"))),
            digest: "d".into(),
        }
    }

    /// The payloads are stored data: the typed shapes equal the JSON the earlier hand-built
    /// `json!` produced.
    #[test]
    fn connect_and_discover_payloads_keep_their_stored_shape() {
        let effect = ConnectEffect {
            scope: ConfigScope::Project(ProjectId(id("project"))),
            server_id: id("server"),
            pending_revision: 4,
            catalog_revision: None,
            principal: ActorId(id("agent")),
            principal_kind: PrincipalKind::Agent,
            session_id: Some(id("session")),
            fences: vec![policy_fence()],
        };
        let expected = json!({
            "scope": "project:project", "serverId": "server", "pendingRevision": 4,
            "principal": "agent", "principalKind": "Agent", "sessionId": "session",
            "fences": [policy_fence()],
        });
        assert_eq!(effect.journal(), expected);
        let mut discover = effect.clone();
        discover.catalog_revision = Some(7);
        discover.session_id = None;
        let mut expected = expected;
        expected["catalogRevision"] = json!(7);
        expected["sessionId"] = Value::Null;
        assert_eq!(discover.journal(), expected);
        let parsed = ConnectEffect::parse(&expected).unwrap();
        assert_eq!(parsed.catalog_revision, Some(7));
        assert_eq!(parsed.session_id, None);
        assert_eq!(parsed.principal_kind, PrincipalKind::Agent);
    }

    #[test]
    fn disconnect_queue_and_event_payloads_keep_their_stored_shape() {
        let effect = DisconnectEffect {
            scope: ConfigScope::Workspace,
            server_id: id("server"),
            server: json!({"id": "server"}),
            principal: ActorId(id("human")),
            fences: vec![policy_fence()],
        };
        assert_eq!(
            effect.journal(),
            json!({"scope": "workspace", "serverId": "server", "server": {"id": "server"},
                   "principal": "human", "fences": [policy_fence()]})
        );
        assert_eq!(
            DisconnectEffect::parse(&effect.journal()).unwrap().scope,
            ConfigScope::Workspace
        );
        let server = id("server");
        assert_eq!(
            QueuedEffect {
                server_id: &server,
                call: None
            }
            .payload(),
            json!({"serverId": "server"})
        );
        let scope = ConfigScope::Workspace;
        assert_eq!(
            ServerEvent::new(&server, &scope).payload(),
            json!({"serverId": "server", "scope": "workspace"})
        );
        assert_eq!(
            ServerEvent::new(&server, &scope).catalog(3).payload(),
            json!({"serverId": "server", "scope": "workspace", "catalogRevision": 3})
        );
        assert_eq!(
            SamplingEvent {
                server_id: &server,
                attempt_id: &id("a"),
                ordinal: 2,
                reason: None
            }
            .payload(),
            json!({"serverId": "server", "attemptId": "a", "ordinal": 2, "reason": null})
        );
    }

    #[test]
    fn a_tool_payload_round_trips_and_refuses_a_bad_kind_or_arguments() {
        let digest = "a".repeat(64);
        let journal = json!({
            "scope": "workspace", "serverId": "server", "serverRevision": 2,
            "catalogRevision": 3, "capabilityId": "capability",
            "schema": {"schemaId": "mcp/server.tool", "revision": "1", "sha256": digest},
            "wireName": "tool", "arguments": {"x": 1}, "principal": "agent",
            "principalKind": "Agent", "sessionId": "session", "projectId": null,
            "fences": [policy_fence()],
            "call": {"attemptId": "attempt", "nativeCallId": "call", "ordinal": "1"},
        });
        let parsed = ToolEffect::parse(&journal).unwrap();
        assert_eq!(parsed.wire_name, "tool");
        assert_eq!(parsed.journal(), journal);
        let mut bad = journal.clone();
        bad["arguments"] = json!([1]);
        assert!(ToolEffect::parse(&bad).is_err());
        let mut bad = journal;
        bad["principalKind"] = json!("Nobody");
        assert!(ToolEffect::parse(&bad).is_err());
    }
}
