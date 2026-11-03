//! Operation descriptor registry.
//!
//! Every catalog operation has exactly one [`OperationDescriptor`] (generated into `catalog.rs`
//! from the family register). A descriptor binds the operation name to its family, owning
//! service, request envelope, selector, allowed modes with their authority/effect policy, the
//! decoder for each mode's input type and the result type. The gateway resolves the descriptor
//! from the operation the *host* selected; the message body can only confirm that selection
//! through its discriminator, never choose a different operation.
mod catalog;

use crate::admission::AdmissionError;
use crate::operations::OperationInput;
use crate::protocol::{Check, data, provider};
use schemars::{JsonSchema, Schema};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Request mode. A Query cannot perform a mutation under a read entitlement; a Command is the only
/// mode that forms a receipt identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Mode {
    Query,
    Command,
}
impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "Query",
            Self::Command => "Command",
        }
    }
    /// The envelope member that carries this mode's caller identity.
    pub fn key_member(self) -> &'static str {
        match self {
            Self::Query => "queryId",
            Self::Command => "requestKey",
        }
    }
}

/// Recorded authority class of a mode policy. It names what the authority port must establish;
/// it is never satisfied by anything in the request body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityClass {
    ScopedRead,
    Scoped,
    /// Specialized envelope that keeps its earlier recorded host command authority contract.
    ExistingHostCommand,
    HostMaintenance,
    Configuration,
    HumanConfiguration,
    HumanDecision,
    HumanMaintenance,
}

/// Recorded effect class of a mode policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectClass {
    /// Read-only or a declared, authorized external read.
    ReadOnly,
    Local,
    /// Specialized envelope that keeps its earlier recorded journaled command contract.
    ExistingJournaled,
    Journaled,
    External,
    Inference,
    /// The effective action selected inside the payload decides the effect class.
    ActionDependent,
}

/// Request envelope shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Envelope {
    /// `{mode, protocol, queryId|requestKey, body: {kind, ...}}`.
    Domain,
    /// An earlier recorded envelope (`schema` names its JSON Schema document).
    Specialized { schema: &'static str },
}

/// JSON Pointer whose string value must equal `equals` for the selected operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selector {
    pub pointer: &'static str,
    pub equals: &'static str,
}

/// Registered schema revision of an operation's input/result binding under [`crate::protocol::CONTRACT`].
/// Changing a binding requires a new revision plus an explicit upcaster for stored records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaRevision(pub u32);
impl SchemaRevision {
    pub const INITIAL: Self = Self(1);
}

/// A typed input together with its normalized representation (envelope identity removed).
pub struct Decoded {
    pub(crate) input: OperationInput,
    pub(crate) normalized: Value,
}

pub(crate) type DecodeFn = fn(Value) -> Result<Decoded, AdmissionError>;

/// One allowed mode of an operation.
#[derive(Clone, Copy)]
pub struct ModeBinding {
    pub mode: Mode,
    pub authority: AuthorityClass,
    pub effect: EffectClass,
    /// Rust input type, for diagnostics and generated tool descriptions.
    pub input_type: &'static str,
    pub(crate) decode: DecodeFn,
    input_schema: fn() -> Schema,
}
impl ModeBinding {
    /// JSON Schema of this mode's input (the domain body without `kind`, or the specialized envelope).
    pub fn input_schema(&self) -> Schema {
        (self.input_schema)()
    }
}
impl std::fmt::Debug for ModeBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModeBinding")
            .field("mode", &self.mode)
            .field("authority", &self.authority)
            .field("effect", &self.effect)
            .field("input_type", &self.input_type)
            .finish()
    }
}

/// Registered binding of one catalog operation.
pub struct OperationDescriptor {
    pub name: &'static str,
    pub family: &'static str,
    /// Owning application service.
    pub owner: &'static str,
    pub envelope: Envelope,
    pub selector: Option<Selector>,
    pub modes: &'static [ModeBinding],
    /// Value type carried by this operation's `OperationResult` variant.
    pub result_type: &'static str,
    result_schema: fn() -> Schema,
    pub schema_revision: SchemaRevision,
}
impl OperationDescriptor {
    pub fn binding(&self, mode: Mode) -> Option<&'static ModeBinding> {
        self.modes.iter().find(|b| b.mode == mode)
    }
    pub fn result_schema(&self) -> Schema {
        (self.result_schema)()
    }
}
impl std::fmt::Debug for OperationDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationDescriptor")
            .field("name", &self.name)
            .field("family", &self.family)
            .field("envelope", &self.envelope)
            .field("modes", &self.modes)
            .finish_non_exhaustive()
    }
}

/// Looks up the descriptor of a host-selected operation.
pub fn find(operation: &str) -> Option<&'static OperationDescriptor> {
    catalog::index_of(operation).map(|i| &catalog::CATALOG[i])
}
/// All registered operations in register order.
pub fn all() -> &'static [OperationDescriptor] {
    &catalog::CATALOG
}
/// All registered families in register order.
pub fn families() -> &'static [&'static str] {
    &catalog::FAMILIES
}

fn schema_of<T: JsonSchema>() -> Schema {
    schemars::schema_for!(T)
}

/// Decodes a domain body (with `kind` already removed) into the selected input type, then applies
/// nested bounds and the recorded pure cross-field rules for exactly this operation and mode.
fn decode_domain_body<T: DeserializeOwned + Serialize + Check>(
    operation: &'static str,
    mode: Mode,
    body: Value,
    wrap: fn(T) -> OperationInput,
) -> Result<Decoded, AdmissionError> {
    let input: T =
        serde_json::from_value(body).map_err(|e| AdmissionError::Decode(e.to_string()))?;
    input.check().map_err(AdmissionError::Rule)?;
    let normalized =
        serde_json::to_value(&input).map_err(|e| AdmissionError::Decode(e.to_string()))?;
    crate::protocol::rules::input(operation, mode.as_str(), &normalized)
        .map_err(AdmissionError::Rule)?;
    Ok(Decoded {
        input: wrap(input),
        normalized,
    })
}

/// A recorded specialized envelope: typed decode, its own cross-field validation and
/// normalization. The normalized form drops the contract and caller-identity members, which are
/// bound separately and never part of the intent fingerprint's input.
pub(crate) trait SpecializedEnvelope: DeserializeOwned + Serialize {
    fn validate_envelope(&self) -> Result<(), String>;
    fn normalize(&mut self) {}
}

fn decode_specialized<T: SpecializedEnvelope>(
    value: Value,
    wrap: fn(T) -> OperationInput,
) -> Result<Decoded, AdmissionError> {
    let mut input: T =
        serde_json::from_value(value).map_err(|e| AdmissionError::Decode(e.to_string()))?;
    input.validate_envelope().map_err(AdmissionError::Rule)?;
    input.normalize();
    let mut normalized =
        serde_json::to_value(&input).map_err(|e| AdmissionError::Decode(e.to_string()))?;
    if let Some(map) = normalized.as_object_mut() {
        for member in ["protocol", "requestKey", "queryId"] {
            map.remove(member);
        }
    }
    Ok(Decoded {
        input: wrap(input),
        normalized,
    })
}

macro_rules! specialized {
    ($($ty:ty => $validate:expr),* $(,)?) => {$(
        impl SpecializedEnvelope for $ty {
            fn validate_envelope(&self) -> Result<(), String> {
                let check: fn(&$ty) -> Result<(), &'static str> = $validate;
                check(self).map_err(str::to_owned)
            }
        }
    )*};
}
specialized!(
    crate::protocol::QueryRequest => |v| v.validate(),
    data::GraphReadRequest => |v| v.validate(),
    data::ArtifactCommandRequest => |v| v.validate(),
    data::ArtifactQueryRequest => |v| v.validate(),
    data::ArtifactDeletionRequest => |_| Ok(()),
    data::GitCommandRequest => |v| v.validate(),
    data::IndexCommandRequest => |_| Ok(()),
    data::ReleaseCommandRequest => |v| v.validate(),
    data::RetrievalRequest => |v| v.validate(),
    provider::SessionCommandRequest => |v| v.validate(),
    provider::SessionQueryRequest => |v| v.validate(),
    crate::protocol::addons::AddonCommandRequest => |v| v.validate(),
);
impl SpecializedEnvelope for crate::protocol::CommandRequest {
    fn validate_envelope(&self) -> Result<(), String> {
        self.validate().map_err(str::to_owned)
    }
    /// Covered hold references are a set: sort by ID before fingerprinting.
    fn normalize(&mut self) {
        crate::protocol::CommandRequest::normalize(self)
    }
}
