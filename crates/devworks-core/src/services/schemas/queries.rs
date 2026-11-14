//! Keyset pages bind the selected immutable view, exact filters, source ACL revision and policy.
use super::{SchemaService, access};
use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    operations::{OperationInput, schemas as ops},
    ports::{IdKind, Preparation, Prepared, QueryAnswer, QueryHandler, storage::*},
    protocol::{
        CorrelationId, DomainErrorCode, DomainFault, Id,
        data::{DataScope, ViewSelection},
        records::*,
        results::OperationResult,
        support::Page,
    },
    services::{
        cursor::{self, CursorBinding},
        sessions::{Failure, Refusal, query_fault, rev},
    },
};
use serde::Serialize;
use serde_json::Value;

fn invalid(message: impl std::fmt::Display) -> StorageError {
    StorageError::InvalidRecord(message.to_string())
}
fn binding<T: Serialize>(
    ctx: &InvocationContext,
    op: &str,
    input: &T,
    revision: i64,
    view: &str,
) -> Result<CursorBinding, StorageError> {
    let mut input = serde_json::to_value(input).map_err(invalid)?;
    if let Some(page) = input.get_mut("page").and_then(Value::as_object_mut) {
        page.remove("cursor");
    }
    Ok(CursorBinding::new(
        ctx,
        op,
        &[
            &serde_json::to_string(&input).map_err(invalid)?,
            &ctx.policy_revision().get().to_string(),
            view,
        ],
        revision,
    ))
}
/// One schema metadata page: rows after the cursor through the shared pager, mapped to their
/// wire form and clipped to the response frame.
fn page<S, T: Serialize, E: From<StorageError>>(
    binding: &CursorBinding,
    request: &PageRequest,
    mut fetch: impl FnMut(Option<&Id>, u16) -> Result<Vec<S>, StorageError>,
    stored_id: impl Fn(&S) -> &Id,
    wire: impl FnMut(S) -> Result<T, E>,
    id: impl Fn(&T) -> &Id,
) -> Result<Page<T>, E> {
    let rows = cursor::page(
        binding,
        request,
        |after, n| {
            let after = after
                .map(|s| Id::new(s).map_err(|_| StorageError::CursorExpired))
                .transpose()?;
            fetch(after.as_ref(), n)
        },
        |r| stored_id(r).as_str().to_owned(),
    )?;
    Ok(rows
        .try_map(wire)?
        .clip_to_frame(binding, crate::framing::MAX_FRAME_BYTES - 16_384, |r| {
            id(r).as_str().to_owned()
        })?
        .into_page())
}
fn view(
    read: &dyn ReadSnapshot,
    source: &StoredSchemaSource,
    selected: &ViewSelection,
    ctx: &InvocationContext,
    scope: &DataScope,
    now: i64,
) -> Result<StoredSchemaView, Failure> {
    if let Some(view) = super::pins::selected(read, ctx, scope, source, selected, now)? {
        return Ok(view);
    }
    let unavailable = || {
        Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "schema_view",
            Some(source.connection.id.clone()),
        )
    };
    let id = match selected {
        ViewSelection::SchemaPublished { connection_ids }
            if connection_ids.len() == 1 && connection_ids[0] == source.connection.id =>
        {
            source
                .connection
                .current_view_id
                .as_ref()
                .ok_or_else(unavailable)?
        }
        ViewSelection::Exact { view_ids } if view_ids.len() == 1 => &view_ids[0],
        _ => return Err(unavailable().into()),
    };
    read.schemas()
        .schema_view(id)?
        .filter(|v| {
            v.connection_id == source.connection.id
                && v.configuration_revision == source.configuration_revision
                && v.sealed
        })
        .ok_or_else(|| unavailable().into())
}
impl SchemaService {
    fn connections(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ConnectionsInput,
        prepared: Option<&super::pins::PreparedViews>,
    ) -> Result<(OperationResult, i64), Failure> {
        let visibility = access::visibility(read, ctx, &input.scope)?;
        let revision = read.schemas().schema_inventory_revision()?;
        let binding = binding(ctx, "schemas.connections", input, revision, "")?;
        let now = self.clock.now_ms().get();
        let result = page(
            &binding,
            &input.page,
            |after, n| read.schemas().schema_sources(&visibility, after, n),
            |r| &r.connection.id,
            |r| {
                let mut row = access::project(&r, &visibility);
                row.read_view =
                    super::pins::projection(read, ctx, &input.scope, &r, prepared, now)?;
                Ok::<_, Failure>(row)
            },
            |r: &SchemaConnection| &r.id,
        )?;
        Ok((
            OperationResult::SchemasConnections { value: result },
            revision,
        ))
    }
    fn selected(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: &DataScope,
        id: &Id,
        selection: &ViewSelection,
    ) -> Result<(StoredSchemaSource, StoredSchemaView), Failure> {
        let visibility = access::visibility(read, ctx, scope)?;
        let source = access::source(read, id, &visibility)?;
        let view = view(
            read,
            &source,
            selection,
            ctx,
            scope,
            self.clock.now_ms().get(),
        )?;
        Ok((source, view))
    }
}
impl QueryHandler for SchemaService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let result: Result<Option<Prepared>, Failure> = (|| {
            let OperationInput::Schemas(ops::Input::Connections(input)) = query.input() else {
                return Ok(None);
            };
            if query.schema_revision().0 < 5 || self.runtime.is_none() {
                return Ok(None);
            }
            if env.stopped(self.clock.now_ms().get()) {
                return Err(StorageError::Cancelled.into());
            }
            let (sources, visibility) = env.read(|read| {
                Ok((|| -> Result<_, Failure> {
                    let visibility = access::visibility(read, ctx, &input.scope)?;
                    let (OperationResult::SchemasConnections { value }, _) =
                        self.connections(read, ctx, input, None)?
                    else {
                        unreachable!()
                    };
                    let sources = value
                        .items
                        .0
                        .iter()
                        .map(|r| read.schemas().schema_source(&r.id))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter()
                        .flatten()
                        .collect();
                    Ok((sources, visibility))
                })())
            })??;
            self.prepare_connection_views(ctx, query, env, &input.scope, sources, visibility)
        })();
        result.map_err(|e| {
            query_fault(
                e,
                "schema",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result: Result<(OperationResult, i64), Failure> = (|| match query.input() {
            OperationInput::Schemas(ops::Input::Connections(input)) => self.connections(
                read,
                ctx,
                input,
                query.prepared::<super::pins::PreparedViews>(),
            ),
            OperationInput::Schemas(ops::Input::Objects(input)) => {
                let (source, view) =
                    self.selected(read, ctx, &input.scope, &input.connection_id, &input.views)?;
                let revision = source.configuration_revision;
                let binding = binding(
                    ctx,
                    "schemas.objects",
                    input,
                    revision,
                    &format!(
                        "{}:{}:{}",
                        view.id.as_str(),
                        view.digest,
                        source.configuration_revision
                    ),
                )?;
                let filter = SchemaObjectFilter {
                    object_id: input.object_id.clone(),
                    kinds: input
                        .kinds
                        .0
                        .iter()
                        .map(|k| {
                            serde_json::to_value(k)
                                .expect("closed enum")
                                .as_str()
                                .expect("enum string")
                                .to_owned()
                        })
                        .collect(),
                    parent_id: input.parent_id.clone(),
                    text: input.text.as_ref().map(|t| t.0.clone()),
                };
                let result = page(
                    &binding,
                    &input.page,
                    |after, n| read.schemas().schema_objects(&view.id, &filter, after, n),
                    |r: &SchemaObject| &r.id,
                    Ok::<_, StorageError>,
                    |r: &SchemaObject| &r.id,
                )?;
                Ok((OperationResult::SchemasObjects { value: result }, revision))
            }
            OperationInput::Schemas(ops::Input::Relationships(input)) => {
                let (source, view) =
                    self.selected(read, ctx, &input.scope, &input.connection_id, &input.views)?;
                let revision = source.configuration_revision;
                let binding = binding(
                    ctx,
                    "schemas.relationships",
                    input,
                    revision,
                    &format!(
                        "{}:{}:{}",
                        view.id.as_str(),
                        view.digest,
                        source.configuration_revision
                    ),
                )?;
                let result = page(
                    &binding,
                    &input.page,
                    |after, n| {
                        read.schemas()
                            .schema_relations(&view.id, &input.object_ids.0, after, n)
                    },
                    |r: &SchemaRelation| &r.id,
                    Ok::<_, StorageError>,
                    |r: &SchemaRelation| &r.id,
                )?;
                Ok((
                    OperationResult::SchemasRelationships { value: result },
                    revision,
                ))
            }
            _ => Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "schema_query", None).into(),
            ),
        })();
        let (result, revision) = result.map_err(|e| {
            query_fault(
                e,
                "schema",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}
