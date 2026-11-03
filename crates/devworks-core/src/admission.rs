//! Pure request admission: bytes to a typed, normalized, fingerprinted input of exactly the
//! host-selected operation and mode. No I/O, clock, ID source or authority is consulted here.
//!
//! Order (each step runs only if every earlier step passed):
//! 1. strict framing ([`crate::framing`]): byte bound, UTF-8, single value, no duplicate members,
//!    nesting bound, finite numbers;
//! 2. contract version: `protocol` must be exactly [`CONTRACT`]; an unknown version is rejected
//!    before anything else in the frame is interpreted, so it can never grant anything;
//! 3. operation selection: the host-selected operation must be registered and allow the selected
//!    mode; a domain envelope's `mode` must equal it; the selector member must name the selected
//!    operation (shared input shapes never merge entitlements); the optional `schemaRevision`
//!    member must name a revision the operation registers (absent means 1). Any other revision is
//!    refused here, before authority and before any receipt exists;
//! 4. caller identity member (`queryId` for a Query, `requestKey` for a Command);
//! 5. strict typed decode into the selected input type at the negotiated revision (unknown
//!    members and members introduced after that revision rejected), nested bounds and the
//!    recorded pure cross-field rules;
//! 6. normalization and the intent [`Fingerprint`];
//! 7. effective-action classification: an `ActionDependent` binding is resolved to the effect
//!    class of the action the typed input selects (AD69), so authority and receipt admission see
//!    the concrete effect. An unclassified action-dependent binding is refused (fail closed).
use crate::fingerprint::{self, Fingerprint};
use crate::framing::{self, FrameError};
use crate::operations::{OperationInput, browser, github};
use crate::protocol::records::{AccountAction, BrowserSessionAction};
use crate::protocol::{CONTRACT, Id, QueryId, RequestKey};
use crate::registry::{
    self, EffectClass, Envelope, Mode, ModeBinding, OperationDescriptor, SchemaRevision,
};
use serde_json::{Map, Value};
use std::fmt;

/// The operation and mode chosen by the host adapter (UI action binding, tool registration,
/// schedule definition, package contribution) — never by the message body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection<'a> {
    pub operation: &'a str,
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Frame(FrameError),
    NotAnObject,
    MissingProtocol,
    UnsupportedProtocol(String),
    UnknownOperation(String),
    ModeNotAllowed {
        operation: &'static str,
        mode: Mode,
    },
    /// The request negotiated a schema revision the selected operation does not register.
    UnsupportedSchemaRevision {
        operation: &'static str,
        revision: u64,
    },
    EnvelopeModeMismatch {
        selected: Mode,
    },
    SelectorMismatch {
        pointer: &'static str,
        expected: &'static str,
    },
    /// Missing/invalid caller identity member or a member outside the envelope.
    InvalidEnvelope(String),
    /// Strict typed decoding failed (unknown member, wrong type, scalar or bound violation).
    Decode(String),
    /// A recorded cross-field rule failed.
    Rule(String),
    Fingerprint(String),
}
impl AdmissionError {
    /// The same error with credential-shaped content in its caller-derived text redacted. The
    /// gateway returns only redacted admission errors.
    pub fn redacted(self) -> Self {
        let r = |s: String| crate::authority::redact::Redactor::new().redact_text(&s);
        match self {
            Self::UnsupportedProtocol(s) => Self::UnsupportedProtocol(r(s)),
            Self::UnknownOperation(s) => Self::UnknownOperation(r(s)),
            Self::InvalidEnvelope(s) => Self::InvalidEnvelope(r(s)),
            Self::Decode(s) => Self::Decode(r(s)),
            Self::Rule(s) => Self::Rule(r(s)),
            Self::Fingerprint(s) => Self::Fingerprint(r(s)),
            other => other,
        }
    }
}
impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(e) => e.fmt(f),
            Self::NotAnObject => f.write_str("request frame is not an object"),
            Self::MissingProtocol => f.write_str("request has no protocol string"),
            Self::UnsupportedProtocol(p) => write!(f, "unsupported contract revision {p:?}"),
            Self::UnknownOperation(op) => write!(f, "unknown operation {op:?}"),
            Self::ModeNotAllowed { operation, mode } => {
                write!(f, "{operation} does not allow {} mode", mode.as_str())
            }
            Self::UnsupportedSchemaRevision {
                operation,
                revision,
            } => write!(f, "{operation} does not support schema revision {revision}"),
            Self::EnvelopeModeMismatch { selected } => {
                write!(
                    f,
                    "envelope mode does not match selected {}",
                    selected.as_str()
                )
            }
            Self::SelectorMismatch { pointer, expected } => {
                write!(f, "{pointer} must select {expected}")
            }
            Self::InvalidEnvelope(e) => write!(f, "invalid envelope: {e}"),
            Self::Decode(e) => write!(f, "invalid input: {e}"),
            Self::Rule(e) => write!(f, "rule violation: {e}"),
            Self::Fingerprint(e) => write!(f, "cannot fingerprint: {e}"),
        }
    }
}

/// The caller identity a reply can be addressed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallerKey {
    Query(QueryId),
    Command(RequestKey),
}

/// An admission failure plus the caller identity, when the frame carried a valid one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub error: AdmissionError,
    pub key: Option<CallerKey>,
}

/// A fully decoded request for the selected operation.
#[derive(Debug)]
pub struct DecodedRequest {
    pub descriptor: &'static OperationDescriptor,
    pub binding: &'static ModeBinding,
    pub key: CallerKey,
    pub input: OperationInput,
    pub fingerprint: Fingerprint,
    /// The negotiated schema revision: it selects the strict decoder, enters the fingerprint and
    /// the receipt, and decides the result projection.
    pub revision: SchemaRevision,
    /// Effective effect class (never `ActionDependent`).
    pub effect: EffectClass,
    /// Effective action label for action-dependent bindings.
    pub action: Option<&'static str>,
}
impl DecodedRequest {
    pub fn mode(&self) -> Mode {
        self.binding.mode
    }
}

/// Runs admission steps 1–6 for `raw` under the host `selection`.
pub fn decode(selection: Selection<'_>, raw: &[u8]) -> Result<DecodedRequest, Refusal> {
    let value = framing::parse(raw).map_err(|e| Refusal {
        error: AdmissionError::Frame(e),
        key: None,
    })?;
    let Value::Object(mut object) = value else {
        return Err(Refusal {
            error: AdmissionError::NotAnObject,
            key: None,
        });
    };
    let key = caller_key(&object, selection.mode);
    let refuse = |error| Refusal {
        error,
        key: key.clone(),
    };

    match object.get("protocol") {
        Some(Value::String(p)) if p == CONTRACT => {}
        Some(Value::String(p)) => {
            return Err(refuse(AdmissionError::UnsupportedProtocol(p.clone())));
        }
        _ => return Err(refuse(AdmissionError::MissingProtocol)),
    }

    let descriptor = registry::find(selection.operation).ok_or_else(|| {
        refuse(AdmissionError::UnknownOperation(
            selection.operation.to_owned(),
        ))
    })?;
    let binding = descriptor.binding(selection.mode).ok_or_else(|| {
        refuse(AdmissionError::ModeNotAllowed {
            operation: descriptor.name,
            mode: selection.mode,
        })
    })?;
    let revision = negotiated_revision(&mut object, descriptor).map_err(refuse)?;
    if descriptor.envelope == Envelope::Domain
        && object.get("mode").and_then(Value::as_str) != Some(selection.mode.as_str())
    {
        return Err(refuse(AdmissionError::EnvelopeModeMismatch {
            selected: selection.mode,
        }));
    }
    let value = Value::Object(object);
    if let Some(selector) = descriptor.selector
        && value.pointer(selector.pointer).and_then(Value::as_str) != Some(selector.equals)
    {
        return Err(refuse(AdmissionError::SelectorMismatch {
            pointer: selector.pointer,
            expected: selector.equals,
        }));
    }
    let Some(key) = key.clone() else {
        return Err(refuse(AdmissionError::InvalidEnvelope(format!(
            "{} must be a valid opaque identifier",
            selection.mode.key_member()
        ))));
    };

    let decoded = match descriptor.envelope {
        Envelope::Domain => {
            let body = domain_body(value, selection.mode).map_err(refuse)?;
            (binding.decode)(body, revision)
        }
        Envelope::Specialized { .. } => (binding.decode)(value, revision),
    }
    .map_err(refuse)?;
    // Defense in depth: the generated decoder must have produced the selected binding.
    if decoded.input.operation() != descriptor.name || decoded.input.mode() != binding.mode {
        return Err(refuse(AdmissionError::Decode(
            "decoder produced a different operation".into(),
        )));
    }
    let fingerprint = fingerprint::compute(descriptor, binding.mode, revision, &decoded.normalized)
        .map_err(|e| refuse(AdmissionError::Fingerprint(e.to_string())))?;
    let (effect, action) = effective_effect(binding, &decoded.input).map_err(refuse)?;
    Ok(DecodedRequest {
        descriptor,
        binding,
        key,
        input: decoded.input,
        fingerprint,
        revision,
        effect,
        action,
    })
}

/// Takes the envelope's `schemaRevision` member (absent means [`SchemaRevision::INITIAL`]) and
/// checks that the selected operation registers it. The member is envelope metadata: it never
/// reaches the typed decoder, and the fingerprint binds the revision itself.
fn negotiated_revision(
    object: &mut Map<String, Value>,
    descriptor: &'static OperationDescriptor,
) -> Result<SchemaRevision, AdmissionError> {
    let Some(member) = object.remove("schemaRevision") else {
        return Ok(SchemaRevision::INITIAL);
    };
    let revision = member.as_u64().filter(|r| *r >= 1).ok_or_else(|| {
        AdmissionError::InvalidEnvelope("schemaRevision must be a positive integer".into())
    })?;
    u32::try_from(revision)
        .ok()
        .map(SchemaRevision)
        .filter(|r| descriptor.supports(*r))
        .ok_or(AdmissionError::UnsupportedSchemaRevision {
            operation: descriptor.name,
            revision,
        })
}

/// Resolves the effect class a typed input actually requests. For every binding other than
/// `ActionDependent` this is the registered class. The two action-dependent commands are
/// classified here from their typed action (AD69); any other action-dependent binding is refused.
pub fn effective_effect(
    binding: &ModeBinding,
    input: &OperationInput,
) -> Result<(EffectClass, Option<&'static str>), AdmissionError> {
    if binding.effect != EffectClass::ActionDependent {
        return Ok((binding.effect, None));
    }
    let classified = match input {
        OperationInput::Github(github::Input::AccountCommand(i)) => Some(match i.action {
            // Native host credential flows talk to GitHub.
            AccountAction::BeginSignIn => (EffectClass::External, "BeginSignIn"),
            AccountAction::CompleteSignIn => (EffectClass::External, "CompleteSignIn"),
            AccountAction::Refresh => (EffectClass::External, "Refresh"),
            AccountAction::Disconnect => (EffectClass::External, "Disconnect"),
            // Refused earlier as a Command by the recorded cross-field rule (inspection is the
            // Query); classified for exhaustiveness only.
            AccountAction::Inspect => (EffectClass::ReadOnly, "Inspect"),
        }),
        // Creating or closing an owned browser profile/process tree is a journaled command.
        OperationInput::Browser(browser::Input::SessionsCommand(i)) => Some(match i.action {
            BrowserSessionAction::Create { .. } => (EffectClass::Journaled, "Create"),
            BrowserSessionAction::Close { .. } => (EffectClass::Journaled, "Close"),
        }),
        _ => None,
    };
    classified
        .map(|(effect, action)| (effect, Some(action)))
        .ok_or_else(|| {
            AdmissionError::Rule(format!(
                "{} has no effective-action classification",
                input.operation()
            ))
        })
}

/// Best-effort caller identity, used only to address a reply; it is still validated as an opaque ID.
fn caller_key(object: &Map<String, Value>, mode: Mode) -> Option<CallerKey> {
    let raw = object.get(mode.key_member())?.as_str()?;
    let id = Id::new(raw).ok()?;
    Some(match mode {
        Mode::Query => CallerKey::Query(QueryId(id)),
        Mode::Command => CallerKey::Command(RequestKey(id)),
    })
}

/// Checks the closed domain envelope and returns its body without the `kind` discriminator.
fn domain_body(value: Value, mode: Mode) -> Result<Value, AdmissionError> {
    let Value::Object(mut object) = value else {
        return Err(AdmissionError::NotAnObject);
    };
    if let Some(extra) = object
        .keys()
        .find(|k| !["mode", "protocol", mode.key_member(), "body"].contains(&k.as_str()))
    {
        return Err(AdmissionError::InvalidEnvelope(format!(
            "unknown envelope member {extra:?}"
        )));
    }
    match object.remove("body") {
        Some(Value::Object(mut body)) => {
            body.remove("kind");
            Ok(Value::Object(body))
        }
        _ => Err(AdmissionError::InvalidEnvelope(
            "body must be an object".into(),
        )),
    }
}
