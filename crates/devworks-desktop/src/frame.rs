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
use devworks_core::registry::{self, Mode};
use serde::de::DeserializeOwned;
use serde_json::json;

/// Which schema revision a frame negotiates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Negotiate {
    /// The envelope leaves `schemaRevision` out, which means revision 1.
    Initial,
    /// The newest revision the operation registry lists for the operation.
    Latest,
}

/// The newest schema revision the registry lists for `operation`.
pub fn latest_revision(operation: &str) -> Option<u32> {
    registry::find(operation)?.revisions.last().map(|r| r.0)
}

/// The frame for `input`, or no bytes when the input does not form one. Empty bytes are a frame
/// the gateway rejects, the same way it rejects any other malformed request.
pub fn build(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: Option<OperationInput>,
) -> Vec<u8> {
    input
        .and_then(|input| try_build(backend, negotiate, &input))
        .unwrap_or_default()
}

fn try_build(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: &OperationInput,
) -> Option<Vec<u8>> {
    let operation = input.operation();
    let mode = input.mode();
    registry::find(operation)?.binding(mode)?;
    let mut body = input.body().ok()?;
    body["kind"] = json!(operation);
    let mut frame = match mode {
        Mode::Query => {
            json!({"mode": "Query", "protocol": PROTOCOL, "queryId": backend.token("q"), "body": body})
        }
        Mode::Command => {
            json!({"mode": "Command", "protocol": PROTOCOL, "requestKey": backend.token("rk"), "body": body})
        }
    };
    if negotiate == Negotiate::Latest {
        frame["schemaRevision"] = json!(latest_revision(operation)?);
    }
    Some(frame.to_string().into_bytes())
}

/// [`build`] for an input made by a closure, so the closure can use `?` on every conversion.
pub fn with(
    backend: &dyn Backend,
    negotiate: Negotiate,
    input: impl FnOnce() -> Option<OperationInput>,
) -> Vec<u8> {
    build(backend, negotiate, input())
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
