//! Operation descriptor registry.
//!
//! Every catalog operation has exactly one [`OperationDescriptor`] (generated into `catalog.rs`
//! from the family register). A descriptor binds the operation name to its family, owning
//! service, request envelope, selector, allowed modes with their authority/effect policy, the
//! decoder for each mode's input type, the result type and the schema revisions it negotiates.
//! The gateway resolves the descriptor from the operation the *host* selected; the message body
//! can only confirm that selection through its discriminator, never choose a different operation.
//!
//! # Schema revisions
//!
//! An operation lists every schema revision it accepts ([`OperationDescriptor::revisions`]); a
//! request names one with the optional envelope member `schemaRevision` (absent means 1) and
//! admission refuses any other before authority. Revisions are additive: a member introduced at
//! revision N is optional, annotated `x-since: N` in its JSON Schema and handled by
//! [`Revisioned`], so one typed input/result serves every revision. Each revision still has its
//! own strict decoder (a member introduced after the negotiated revision is refused, never
//! discarded), its own JSON Schemas ([`ModeBinding::input_schema`],
//! [`OperationDescriptor::result_schema`]) and its own result projection
//! ([`Revisioned::retain_revision`] on the result). A change that is not additive needs a new
//! input/result type and an explicit upcaster for stored records; none exists yet.
mod catalog;

use crate::admission::AdmissionError;
use crate::operations::OperationInput;
use crate::protocol::support::Revisioned;
use crate::protocol::{Check, data, provider};
use schemars::{JsonSchema, Schema};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeSet;

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
/// An additive change is a new revision with members gated by `x-since`; any other change
/// requires a new binding plus an explicit upcaster for stored records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaRevision(pub u32);
impl SchemaRevision {
    /// The revision of a request that does not negotiate one.
    pub const INITIAL: Self = Self(1);
    /// The largest revision the envelope member `schemaRevision` can name.
    pub const MAX: Self = Self(65535);
}

/// A typed input together with its normalized representation (envelope identity removed).
pub struct Decoded {
    pub(crate) input: OperationInput,
    pub(crate) normalized: Value,
}

pub(crate) type DecodeFn = fn(Value, SchemaRevision) -> Result<Decoded, AdmissionError>;

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
    /// JSON Schema of this mode's input at `revision` (the domain body without `kind`, or the
    /// specialized envelope without its `schemaRevision` member).
    pub fn input_schema(&self, revision: SchemaRevision) -> Schema {
        schema_at((self.input_schema)(), revision)
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
    /// The register binds an `OperationCompletion` contract for asynchronous outcomes. Only such
    /// an operation may answer a command with `Accepted` (a durable operation still running).
    pub completion: bool,
    /// Every schema revision this operation accepts, ascending; the first is always 1.
    pub revisions: &'static [SchemaRevision],
}
impl OperationDescriptor {
    pub fn binding(&self, mode: Mode) -> Option<&'static ModeBinding> {
        self.modes.iter().find(|b| b.mode == mode)
    }
    /// JSON Schema of this operation's result value at `revision`.
    pub fn result_schema(&self, revision: SchemaRevision) -> Schema {
        schema_at((self.result_schema)(), revision)
    }
    /// Whether a request may negotiate `revision` for this operation.
    pub fn supports(&self, revision: SchemaRevision) -> bool {
        self.revisions.contains(&revision)
    }
}
impl std::fmt::Debug for OperationDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationDescriptor")
            .field("name", &self.name)
            .field("family", &self.family)
            .field("envelope", &self.envelope)
            .field("modes", &self.modes)
            .field("revisions", &self.revisions)
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

/// A schema at `revision`: properties annotated `x-since` later than `revision` (and the
/// definitions only they reference) are removed, which is that revision's exact shape.
pub fn schema_at(schema: Schema, revision: SchemaRevision) -> Schema {
    fn later(v: &Value, revision: u32) -> bool {
        v.get("x-since")
            .and_then(Value::as_u64)
            .is_some_and(|s| s > u64::from(revision))
    }
    fn strip(v: &mut Value, revision: u32) {
        match v {
            Value::Object(m) => {
                let mut gone = Vec::new();
                if let Some(Value::Object(props)) = m.get_mut("properties") {
                    gone = props
                        .iter()
                        .filter(|(_, p)| later(p, revision))
                        .map(|(k, _)| k.clone())
                        .collect();
                    props.retain(|k, _| !gone.contains(k));
                }
                if let Some(Value::Array(required)) = m.get_mut("required") {
                    required.retain(|r| !gone.iter().any(|g| r == g));
                }
                m.values_mut().for_each(|x| strip(x, revision));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| strip(x, revision)),
            _ => {}
        }
    }
    fn refs(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                if let Some(name) = m
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                {
                    out.insert(name.to_owned());
                }
                m.values().for_each(|x| refs(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }
    let mut v = schema.to_value();
    strip(&mut v, revision.0);
    if let Some(Value::Object(defs)) = v.as_object_mut().and_then(|m| m.remove("$defs")) {
        let mut live = BTreeSet::new();
        refs(&v, &mut live);
        let mut frontier: Vec<String> = live.iter().cloned().collect();
        while let Some(name) = frontier.pop() {
            let mut next = BTreeSet::new();
            if let Some(d) = defs.get(&name) {
                refs(d, &mut next);
            }
            frontier.extend(next.into_iter().filter(|n| live.insert(n.clone())));
        }
        let kept: serde_json::Map<String, Value> =
            defs.into_iter().filter(|(k, _)| live.contains(k)).collect();
        if !kept.is_empty() {
            v["$defs"] = Value::Object(kept);
        }
    }
    Schema::try_from(v).expect("a stripped schema object is still a schema")
}

/// The strict reader of `revision`: a member introduced after it is refused, never discarded.
fn admit_revision<T: Revisioned + Clone + Serialize>(
    input: &T,
    revision: SchemaRevision,
) -> Result<(), AdmissionError> {
    let mut at = input.clone();
    at.retain_revision(revision.0);
    let encode = |v: &T| serde_json::to_value(v).map_err(|e| AdmissionError::Decode(e.to_string()));
    if encode(&at)? != encode(input)? {
        return Err(AdmissionError::Decode(format!(
            "a member introduced after schema revision {} is present",
            revision.0
        )));
    }
    Ok(())
}

/// Decodes a domain body (with `kind` already removed) into the selected input type at the
/// negotiated revision, then applies nested bounds and the recorded pure cross-field rules for
/// exactly this operation and mode.
fn decode_domain_body<T: DeserializeOwned + Serialize + Check + Revisioned + Clone>(
    operation: &'static str,
    mode: Mode,
    revision: SchemaRevision,
    body: Value,
    wrap: fn(T) -> OperationInput,
) -> Result<Decoded, AdmissionError> {
    let input: T =
        serde_json::from_value(body).map_err(|e| AdmissionError::Decode(e.to_string()))?;
    admit_revision(&input, revision)?;
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
///
/// [`SpecializedEnvelope::retain_revision`] is the envelope's strict revision reader and has no
/// default: every envelope states it, so a member gated with `x-since` is never admitted at an
/// earlier revision through an inherited no-op. The recorded-fixture tests refuse every `x-since`
/// member of every envelope schema at the revision before it.
pub(crate) trait SpecializedEnvelope: DeserializeOwned + Serialize + Clone {
    fn validate_envelope(&self) -> Result<(), String>;
    fn normalize(&mut self) {}
    fn retain_revision(&mut self, revision: u32);
}
/// Adapts a specialized envelope to the strict revision reader.
#[derive(Clone, Serialize)]
#[serde(transparent)]
struct Revised<T>(T);
impl<T: SpecializedEnvelope> Revisioned for Revised<T> {
    fn retain_revision(&mut self, revision: u32) {
        self.0.retain_revision(revision)
    }
}

fn decode_specialized<T: SpecializedEnvelope>(
    value: Value,
    revision: SchemaRevision,
    wrap: fn(T) -> OperationInput,
) -> Result<Decoded, AdmissionError> {
    let input = Revised(
        serde_json::from_value::<T>(value).map_err(|e| AdmissionError::Decode(e.to_string()))?,
    );
    admit_revision(&input, revision)?;
    let Revised(mut input) = input;
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

/// Envelopes whose every member exists since revision 1: their revision reader keeps everything.
macro_rules! specialized {
    ($($ty:ty => $validate:expr),* $(,)?) => {$(
        impl SpecializedEnvelope for $ty {
            fn validate_envelope(&self) -> Result<(), String> {
                let check: fn(&$ty) -> Result<(), &'static str> = $validate;
                check(self).map_err(str::to_owned)
            }
            fn retain_revision(&mut self, _revision: u32) {}
        }
    )*};
}
specialized!(
    crate::protocol::QueryRequest => |v| v.validate(),
    data::GraphReadRequest => |v| v.validate(),
    data::ArtifactDeletionRequest => |_| Ok(()),
    data::GitCommandRequest => |v| v.validate(),
    data::IndexCommandRequest => |_| Ok(()),
    data::ReleaseCommandRequest => |v| v.validate(),
    data::RetrievalRequest => |v| v.validate(),
    provider::SessionCommandRequest => |v| v.validate(),
    provider::SessionQueryRequest => |v| v.validate(),
    crate::protocol::addons::AddonCommandRequest => |v| v.validate(),
);
/// Revision 2 adds artifact tags (a set: decoding sorts it, an empty set is no tags).
macro_rules! revisioned_artifact {
    ($($ty:ty),*) => {$(
        impl SpecializedEnvelope for $ty {
            fn validate_envelope(&self) -> Result<(), String> {
                self.validate().map_err(str::to_owned)
            }
            fn normalize(&mut self) {
                <$ty>::normalize(self)
            }
            fn retain_revision(&mut self, revision: u32) {
                Revisioned::retain_revision(self, revision)
            }
        }
    )*};
}
revisioned_artifact!(data::ArtifactCommandRequest, data::ArtifactQueryRequest);
impl SpecializedEnvelope for crate::protocol::CommandRequest {
    fn validate_envelope(&self) -> Result<(), String> {
        self.validate().map_err(str::to_owned)
    }
    /// Covered hold references are a set: sort by ID before fingerprinting.
    fn normalize(&mut self) {
        crate::protocol::CommandRequest::normalize(self)
    }
    /// Every member exists since revision 1.
    fn retain_revision(&mut self, _revision: u32) {}
}
