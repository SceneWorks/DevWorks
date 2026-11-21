//! Database source frames: the shared `schemas.*` operations at wire revision 5.
use crate::{
    backend::{Backend, screen_slot},
    frame::{self, Negotiate},
};
use devworks_core::operations::{OperationInput as In, schemas};
use devworks_core::protocol::{
    Id,
    data::ViewSelection,
    records::{PageRequest, SchemaConnection, SchemaSourceAction},
    support::BoundedList,
};

/// The Databases requests the screen waits for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DatabasesSlot {
    /// A source inventory page (list sequence, appended to the shown page).
    DatabaseList(u64, bool),
    /// A schema object page (read sequence, appended).
    DatabaseObjects(u64, bool),
    /// A relationship page (read sequence, appended).
    DatabaseRelations(u64, bool),
    /// A page of the code that uses an object (read sequence, appended).
    DatabaseCodeReferences(u64, bool),
    /// Staging a source's password before its configure command.
    DatabasePrepare,
    /// Preparing a certificate file before its `artifacts.save`.
    DatabaseCaPrepare,
    /// The certificate's `artifacts.save`.
    DatabaseCa,
    /// A source command (its operation).
    DatabaseCommand(String),
    DatabaseData(u64),
    DatabaseDataStatus,
}
screen_slot!(DatabasesSlot, Databases);

/// The frame of a `schemas.*` request (at the newest revision). A request that did not convert
/// (`None`) is no bytes, which the gateway rejects like any malformed request.
pub fn query(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
pub fn command(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
fn page(cursor: Option<&str>) -> Option<PageRequest> {
    frame::page(100, cursor)
}
fn pinned(pin: &str) -> Option<ViewSelection> {
    Some(ViewSelection::Pinned {
        pin_id: frame::id(pin)?,
    })
}
/// `schemas.connections` of a scope, one page.
pub fn connections(project: Option<&str>, cursor: Option<&str>) -> Option<In> {
    Some(In::Schemas(schemas::Input::Connections(
        schemas::ConnectionsInput {
            scope: frame::scope(project)?,
            page: page(cursor)?,
        },
    )))
}
/// `schemas.objects` of one source, as the Databases screen asks for it.
pub struct ObjectsRead {
    pub project: Option<String>,
    pub pin: String,
    pub connection: Id,
    /// One object kind, or empty for every kind.
    pub kind: String,
    /// Text to find, or empty for none.
    pub text: String,
    pub object: Option<String>,
    pub parent: Option<String>,
    pub cursor: Option<String>,
}
impl ObjectsRead {
    pub fn input(&self) -> Option<In> {
        Some(In::Schemas(schemas::Input::Objects(
            schemas::ObjectsInput {
                scope: frame::scope(self.project.as_deref())?,
                views: pinned(&self.pin)?,
                connection_id: self.connection.clone(),
                kinds: BoundedList(if self.kind.is_empty() {
                    Vec::new()
                } else {
                    vec![frame::text(&self.kind)?]
                }),
                parent_id: frame::lift(self.parent.as_deref().map(frame::id))?,
                text: if self.text.is_empty() {
                    None
                } else {
                    Some(frame::text(&self.text)?)
                },
                page: page(self.cursor.as_deref())?,
                object_id: frame::lift(self.object.as_deref().map(frame::id))?,
            },
        )))
    }
}
/// `schemas.code_references` of one object of a source, one page.
pub fn code_references(
    project: Option<&str>,
    pin: &str,
    connection: &Id,
    object: &str,
    cursor: Option<&str>,
) -> Option<In> {
    Some(In::Schemas(schemas::Input::CodeReferences(
        schemas::CodeReferencesInput {
            scope: frame::scope(project)?,
            views: pinned(pin)?,
            connection_id: connection.clone(),
            object_id: frame::id(object)?,
            page: page(cursor)?,
        },
    )))
}
/// `schemas.relationships` among the objects `ids` of one source.
pub fn relationships(
    project: Option<&str>,
    pin: &str,
    connection: &Id,
    ids: &[String],
    cursor: Option<&str>,
) -> Option<In> {
    Some(In::Schemas(schemas::Input::Relationships(
        schemas::RelationshipsInput {
            scope: frame::scope(project)?,
            views: pinned(pin)?,
            connection_id: connection.clone(),
            object_ids: frame::ids(ids)?,
            page: page(cursor)?,
        },
    )))
}
/// `schemas.configure` saving `config` as a new source, or over `existing` (its identity and the
/// revision the edit started from).
pub fn save(
    project: Option<&str>,
    existing: Option<(&str, i64)>,
    config: &serde_json::Value,
) -> Option<In> {
    let (connection_id, expected_revision) = match existing {
        Some((id, revision)) => (Some(frame::id(id)?), Some(frame::number(revision)?)),
        None => (None, None),
    };
    Some(In::Schemas(schemas::Input::Configure(
        schemas::ConfigureInput {
            scope: frame::scope(project)?,
            connection_id,
            expected_revision,
            config: serde_json::from_value(config.clone()).ok()?,
            action: Some(SchemaSourceAction::Save),
        },
    )))
}
/// `schemas.configure` removing `source` from a scope.
pub fn remove_source(project: Option<&str>, source: &SchemaConnection) -> Option<In> {
    Some(In::Schemas(schemas::Input::Configure(
        schemas::ConfigureInput {
            scope: frame::scope(project)?,
            connection_id: Some(source.id.clone()),
            expected_revision: Some(source.revision.clone()),
            config: source.config.clone()?,
            action: Some(SchemaSourceAction::Remove),
        },
    )))
}
/// `schemas.remove_index` of a source at its revision.
pub fn remove_index(source: &SchemaConnection) -> In {
    In::Schemas(schemas::Input::RemoveIndex(schemas::RemoveIndexInput {
        connection_id: source.id.clone(),
        expected_revision: source.revision.clone(),
    }))
}
/// `schemas.refresh` of a source at `revision`, as a connection test when `test_only` is given
/// and true.
pub fn refresh(
    connection: &str,
    revision: &str,
    project: Option<&str>,
    test_only: Option<bool>,
) -> Option<In> {
    Some(In::Schemas(schemas::Input::Refresh(
        schemas::RefreshInput {
            connection_id: frame::id(connection)?,
            expected_revision: frame::text(revision)?,
            test_only,
            scope: Some(frame::scope(project)?),
        },
    )))
}
