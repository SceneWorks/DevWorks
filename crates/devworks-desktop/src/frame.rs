//! The one builder of request frames. A frame is made from the typed input core generated for
//! its operation (an [`OperationInput`]), so a renamed operation, field or input type stops the
//! build instead of changing a request. The operation name and mode come from the input and the
//! schema revision from the operation registry.
use crate::backend::Backend;
use crate::ops::PROTOCOL;
use devworks_core::operations::OperationInput;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::records::PageRequest;
use devworks_core::protocol::support::{BoundedList, Limit};
use devworks_core::protocol::{Id, ProjectId};
use devworks_core::protocol::{Protocol, QueryId, RequestKey};
use devworks_core::registry::{self, Envelope, Mode, SchemaRevision};
use serde::de::DeserializeOwned;
use serde_json::json;

/// Which schema revision a frame negotiates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Negotiate {
    /// The envelope leaves `schemaRevision` out, which means revision 1.
    Initial,
    /// The newest revision the operation registry lists for the operation. Revision 1 is the
    /// envelope default, so an operation that lists only revision 1 sends no member.
    Latest,
    /// An older revision the caller names on purpose, with the reason it stays there. The
    /// registry must still list the revision for the operation; pinning revision 1 sends no
    /// member, as [`Self::Initial`] does.
    Pinned { revision: u32, reason: &'static str },
}
impl Negotiate {
    /// Keeps `operation` at `revision` instead of the registry's newest. `reason` says why the
    /// request must not move, so the pin is a decision in the source and not an accident.
    pub const fn pinned(revision: u32, reason: &'static str) -> Self {
        Self::Pinned { revision, reason }
    }
}

/// The newest schema revision the registry lists for `operation`.
pub fn latest_revision(operation: &str) -> Option<u32> {
    registry::find(operation)?.revisions.last().map(|r| r.0)
}

/// The frame for `input`. `None` input (a caller's own value failed to convert) gives no bytes,
/// which the gateway rejects like any malformed request. An input that is present but forms no
/// frame is a programming error: it asserts in debug builds and logs in release builds.
pub fn build(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: Option<OperationInput>,
) -> Vec<u8> {
    let Some(input) = input else {
        return Vec::new();
    };
    try_build(backend, negotiate, &input).unwrap_or_else(|| {
        // The typed input already proves the operation exists, so reaching here is a programming
        // error: an unregistered operation, an unsupported mode or pin, or an input that does not
        // serialize. Debug builds assert; release builds log and send the empty frame the gateway
        // rejects.
        eprintln!("frame::build formed no frame: the operation, mode or pin is not registered");
        debug_assert!(false, "frame::build could not form a frame");
        Vec::new()
    })
}

fn try_build(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: &OperationInput,
) -> Option<Vec<u8>> {
    let operation = input.operation();
    let mode = input.mode();
    let descriptor = registry::find(operation)?;
    descriptor.binding(mode)?;
    let mut body = input.body().ok()?;
    // A domain envelope wraps the input's members in `body`; a specialized envelope is the input
    // itself (it already holds its protocol and its query ID or request key).
    let mut frame = match descriptor.envelope {
        Envelope::Domain => {
            body["kind"] = json!(operation);
            match mode {
                Mode::Query => {
                    json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"), "body": body})
                }
                Mode::Command => {
                    json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": backend.token("rk"), "body": body})
                }
            }
        }
        Envelope::Specialized { .. } => body,
    };
    match negotiate {
        Negotiate::Initial => {}
        Negotiate::Latest => revise(&mut frame, latest_revision(operation)?),
        Negotiate::Pinned { revision, .. } => {
            descriptor
                .supports(SchemaRevision(revision))
                .then_some(())?;
            revise(&mut frame, revision);
        }
    }
    Some(frame.to_string().into_bytes())
}

fn revise(frame: &mut serde_json::Value, revision: u32) {
    if revision > 1 {
        frame["schemaRevision"] = json!(revision);
    }
}

/// [`build`] for an input made by a closure, so the closure can use `?` on every conversion.
pub fn with(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: impl FnOnce() -> Option<OperationInput>,
) -> Vec<u8> {
    build(backend, negotiate, input())
}

/// `frame` with its request key replaced by `key`, for a command whose replay after recovery must
/// keep one stable key. A frame that is not a JSON object stays empty, as [`build`] leaves it.
pub fn with_request_key(frame: Vec<u8>, key: &str) -> Vec<u8> {
    let Ok(mut envelope) = serde_json::from_slice::<serde_json::Value>(&frame) else {
        return Vec::new();
    };
    envelope["requestKey"] = json!(key);
    serde_json::to_vec(&envelope).expect("a JSON envelope serializes")
}

/// The protocol every specialized request names.
pub const fn protocol() -> Protocol {
    Protocol::Draft1
}
/// A fresh query ID for a specialized query envelope.
pub fn query_id(backend: &dyn Backend) -> Option<QueryId> {
    id(&backend.token("q")).map(QueryId)
}
/// A fresh request key for a specialized command envelope.
pub fn request_key(backend: &dyn Backend) -> Option<RequestKey> {
    id(&backend.token("rk")).map(RequestKey)
}

/// A wire scalar (identifier, bounded text, URL, counter) read from text under the wire type's
/// own validation. `None` when the text is outside the type's contract.
pub fn text<T: DeserializeOwned>(value: &str) -> Option<T> {
    T::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value)).ok()
}
/// A wire counter or bounded integer from a number.
pub fn number<T: DeserializeOwned>(value: i64) -> Option<T> {
    text(&value.to_string())
}
/// A bounded JSON integer (a limit or byte bound, not a string-encoded counter).
pub fn integer<T: DeserializeOwned>(value: i64) -> Option<T> {
    T::deserialize(serde::de::value::I64Deserializer::<serde::de::value::Error>::new(value)).ok()
}
pub fn id(value: &str) -> Option<Id> {
    Id::new(value).ok()
}
/// An optional converted value: an absent input is fine, a present one that does not convert is
/// not.
pub fn lift<T>(value: Option<Option<T>>) -> Option<Option<T>> {
    match value {
        None => Some(None),
        Some(None) => None,
        Some(Some(v)) => Some(Some(v)),
    }
}
/// A list inside its length contract.
pub fn bounded<T, const MIN: usize, const MAX: usize>(
    values: Vec<T>,
) -> Option<BoundedList<T, MIN, MAX>> {
    (MIN..=MAX)
        .contains(&values.len())
        .then_some(BoundedList(values))
}
/// A bounded list of identifiers.
pub fn ids<const MAX: usize>(values: &[String]) -> Option<BoundedList<Id, 0, MAX>> {
    values
        .iter()
        .map(|v| id(v))
        .collect::<Option<Vec<_>>>()
        .map(BoundedList)
}
/// A bounded list of wire scalars read from text.
pub fn texts<T: DeserializeOwned, const MAX: usize>(
    values: &[String],
) -> Option<BoundedList<T, 0, MAX>> {
    values
        .iter()
        .map(|v| text(v))
        .collect::<Option<Vec<_>>>()
        .map(BoundedList)
}
/// The workspace, or the project named by `project`.
pub fn scope(project: Option<&str>) -> Option<DataScope> {
    Some(match project {
        Some(p) => DataScope::Project {
            project_id: ProjectId(id(p)?),
        },
        None => DataScope::Workspace,
    })
}
/// A page of at most `limit` rows after `cursor`.
pub fn page(limit: u32, cursor: Option<&str>) -> Option<PageRequest> {
    Some(PageRequest {
        limit: Limit::new(limit)?,
        cursor: lift(cursor.map(text))?,
    })
}

/// Hand-written requests for tests and recorded fixtures. A request the app sends is never built
/// here: the app builds it through [`build`] from a typed input (the guard test below fails the
/// build when a production source file spells out a request envelope).
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
pub mod fixture {
    use super::*;
    use serde_json::Value;

    /// A domain query frame around `body` (the `kind` member is added).
    pub fn query(backend: &dyn Backend, operation: &str, mut body: Value) -> Vec<u8> {
        body["kind"] = json!(operation);
        json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"), "body": body})
            .to_string()
            .into_bytes()
    }
    /// A domain command frame around `body` (the `kind` member is added).
    pub fn command(backend: &dyn Backend, operation: &str, mut body: Value) -> Vec<u8> {
        body["kind"] = json!(operation);
        json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": backend.token("rk"),
            "body": body})
        .to_string()
        .into_bytes()
    }
    /// `frame` negotiating schema `revision`.
    pub fn at_revision(frame: Vec<u8>, revision: u64) -> Vec<u8> {
        let mut v: Value = serde_json::from_slice(&frame).expect("frames are JSON");
        v["schemaRevision"] = json!(revision);
        v.to_string().into_bytes()
    }
    /// The typed input of `operation` read from a JSON `body` through the registry's decoder, at
    /// the operation's newest revision. Panics when the body is not a valid input.
    pub fn input(operation: &str, mode: Mode, body: Value) -> OperationInput {
        let revision = latest_revision(operation).expect("a registered operation");
        let mut body = body;
        body["kind"] = json!(operation);
        let frame = match mode {
            Mode::Query => json!({"mode": "Query", "protocol": PROTOCOL, "queryId": "q",
                "schemaRevision": revision, "body": body}),
            Mode::Command => json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": "rk",
                "schemaRevision": revision, "body": body}),
        };
        devworks_core::admission::decode(
            devworks_core::admission::Selection { operation, mode },
            frame.to_string().as_bytes(),
        )
        .unwrap_or_else(|e| panic!("{operation} input: {e:?}"))
        .input
    }
    /// [`input`] of a command.
    #[cfg(any(test, feature = "test-support"))]
    pub fn command_input(operation: &str, body: Value) -> OperationInput {
        input(operation, Mode::Command, body)
    }
    /// [`input`] of a query.
    #[cfg(any(test, feature = "test-support"))]
    pub fn query_input(operation: &str, body: Value) -> OperationInput {
        input(operation, Mode::Query, body)
    }
}

#[cfg(test)]
mod guard {
    /// `text` without its `#[cfg(test)] mod ... { ... }` blocks (matched by braces). Other
    /// `#[cfg(test)]` items stay in: only whole test modules are exempt, so production code after
    /// an early `#[cfg(test)]` item is still scanned. A `#[cfg(test)] mod name;` declaration holds
    /// no code in this file, and the walk skips `*_tests.rs` files itself.
    fn strip_test_modules(text: &str) -> String {
        const MARK: &str = "#[cfg(test)]";
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(MARK) {
            out.push_str(&rest[..at]);
            let after = rest[at + MARK.len()..].trim_start();
            let is_mod = after
                .strip_prefix("pub ")
                .unwrap_or(after)
                .trim_start()
                .starts_with("mod ");
            let Some(open) = after.find(['{', ';']).filter(|_| is_mod) else {
                // Not a test module: keep the attribute and the item it gates.
                out.push_str(MARK);
                rest = &rest[at + MARK.len()..];
                continue;
            };
            if after.as_bytes()[open] == b';' {
                rest = &after[open + 1..];
                continue;
            }
            let mut depth = 0usize;
            let mut end = after.len();
            for (i, ch) in after[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn test_module_stripping_keeps_code_after_an_early_cfg_test_item() {
        let text = "fn a() {}\n#[cfg(test)]\nuse x::y;\nfn b() { \"queryId\" }\n#[cfg(test)]\nmod t {\n fn c() { \"requestKey\" { } }\n}\nfn d() {}\n";
        let kept = strip_test_modules(text);
        assert!(kept.contains("queryId"));
        assert!(!kept.contains("requestKey"));
        assert!(kept.contains("fn d()"));
    }

    /// Every request the app sends is built by [`super::build`]. A production source file that
    /// spells out an envelope member or the old raw builders is a hand-built request.
    #[test]
    fn no_production_source_builds_a_request_by_hand() {
        const FORBIDDEN: [&str; 6] = [
            "\"queryId\"",
            "\"requestKey\"",
            "\"schemaRevision\"",
            "query_frame(",
            "command_frame(",
            "at_revision(",
        ];
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                    continue;
                }
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                if !name.ends_with(".rs")
                    || name == "frame.rs"
                    || name.ends_with("_tests.rs")
                    || path.components().any(|c| c.as_os_str() == "frame_golden")
                    || name == "frame_golden.rs"
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let production = strip_test_modules(&text);
                for token in FORBIDDEN {
                    if production.contains(token) {
                        out.push(format!("{} spells out {token}", path.display()));
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut found,
        );
        assert!(
            found.is_empty(),
            "hand-built requests:\n{}",
            found.join("\n")
        );
    }
}
