//! Public wire scalars and references shared by package frames.
//!
//! These intentionally mirror the host's private scalar rules (opaque 1–128 ASCII IDs, canonical
//! signed-64 decimal strings, lowercase SHA-256 digests) instead of depending on `devworks-core`:
//! the package protocol is versioned and released independently of the host internals, and the
//! host converts at its boundary. `devworks-core/tests/extension_scalar_parity.rs` holds each
//! mirrored type and its core counterpart to one corpus and one schema; change both together.
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Opaque identifier: 1–128 ASCII characters from `[A-Za-z0-9_-]`. Never authority.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Id(pub(crate) String);
impl TryFrom<String> for Id {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.is_empty()
            || s.len() > 128
            || !s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            return Err("invalid opaque ID/key");
        }
        Ok(Self(s))
    }
}
impl From<Id> for String {
    fn from(v: Id) -> Self {
        v.0
    }
}
impl Id {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl JsonSchema for Id {
    fn schema_name() -> Cow<'static, str> {
        "Id".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"})
    }
}
macro_rules! ids { ($($name:ident),*) => {$ (
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
    #[serde(transparent)] pub struct $name(pub Id);
)*}; }
ids!(
    ProjectId,
    RootId,
    AssignmentId,
    OperationId,
    AttemptId,
    EventId
);

fn canonical_i64(s: &str) -> Option<i64> {
    if s.is_empty()
        || s.len() > 19
        || !s.bytes().all(|c| c.is_ascii_digit())
        || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse::<i64>().ok()
}

/// Canonical decimal string, `0..=9223372036854775807`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Counter(pub(crate) String);
impl TryFrom<String> for Counter {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        canonical_i64(&s).ok_or("invalid counter")?;
        Ok(Self(s))
    }
}
impl From<Counter> for String {
    fn from(v: Counter) -> Self {
        v.0
    }
}
impl JsonSchema for Counter {
    fn schema_name() -> Cow<'static, str> {
        "Counter".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^(0|[1-9][0-9]{0,18})$","description":"Canonical decimal 0..9223372036854775807. Typed range validation required."})
    }
}

/// Positive canonical decimal string, `1..=9223372036854775807`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(pub(crate) String);
impl TryFrom<String> for Revision {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match canonical_i64(&s) {
            Some(0) => Err("revision must be positive"),
            Some(_) => Ok(Self(s)),
            None => Err("invalid counter"),
        }
    }
}
impl From<Revision> for String {
    fn from(v: Revision) -> Self {
        v.0
    }
}
impl JsonSchema for Revision {
    fn schema_name() -> Cow<'static, str> {
        "Revision".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[1-9][0-9]{0,18}$","description":"Positive signed-64-bit counter; typed upper-bound validation required."})
    }
}

/// Lowercase hexadecimal SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(pub(crate) String);
impl TryFrom<String> for Digest {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid SHA256 digest");
        }
        Ok(Self(s))
    }
}
impl From<Digest> for String {
    fn from(v: Digest) -> Self {
        v.0
    }
}
impl JsonSchema for Digest {
    fn schema_name() -> Cow<'static, str> {
        "Digest".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[a-f0-9]{64}$"})
    }
}

/// `namespace/name` contribution or schema identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedId(pub(crate) String);
impl TryFrom<String> for QualifiedId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let parts: Vec<_> = s.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|p| {
                p.is_empty()
                    || p.len() > 64
                    || !p.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c)
                    })
            })
        {
            return Err("invalid qualified ID");
        }
        Ok(Self(s))
    }
}
impl QualifiedId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<QualifiedId> for String {
    fn from(v: QualifiedId) -> Self {
        v.0
    }
}
impl JsonSchema for QualifiedId {
    fn schema_name() -> Cow<'static, str> {
        "QualifiedId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[a-z0-9_.-]{1,64}/[a-z0-9_.-]{1,64}$"})
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: Id,
    pub version_id: Id,
    pub sha256: Digest,
    pub byte_length: Counter,
    #[schemars(length(min = 1, max = 128))]
    pub media_type: String,
}
impl ArtifactRef {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.media_type.is_empty() || self.media_type.len() > 128 {
            return Err("invalid media type");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaRef {
    pub schema_id: QualifiedId,
    pub revision: Revision,
    pub sha256: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EndpointProfile {
    OpenAiResponses,
    CodexSubscriptionResponses,
    AnthropicMessages,
    ClaudeSubscriptionMessages,
    OpenRouterChat,
    CompatibleChat,
    Embedding,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeBinding {
    pub connection_id: Id,
    pub account_revision: Revision,
    pub endpoint_profile: EndpointProfile,
    pub model_id: Id,
    pub context_epoch: Counter,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCallKey {
    pub attempt_id: AttemptId,
    #[schemars(length(min = 1, max = 1024))]
    pub native_call_id: String,
    pub ordinal: Counter,
}
impl ToolCallKey {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.native_call_id.is_empty() || self.native_call_id.len() > 1024 {
            return Err("invalid native call ID");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteToolCall {
    pub call: ToolCallKey,
    pub capability_id: QualifiedId,
    pub input_schema: SchemaRef,
    pub arguments: ArtifactRef,
    pub native_item: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum TurnOutcome {
    Completed,
    ToolCallsReady,
    Incomplete,
    Cancelled,
    Failed,
    OutcomeUnknown,
}
