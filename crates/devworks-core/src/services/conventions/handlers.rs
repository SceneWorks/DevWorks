//! The same registered owner handles native UI, tools and scheduled requests.
use super::{
    ConventionService, access,
    passages::{DocumentRequest, PreparedDocument},
};
use crate::{
    context::InvocationContext,
    gateway::{AdmittedCommand, AdmittedQuery},
    operations::{OperationInput, conventions as ops},
    ports::{
        CommandHandler, Decided, HandlerFailure, IdKind, Preparation, Prepared, QueryAnswer,
        QueryHandler, storage::*,
    },
    protocol::{CorrelationId, DomainErrorCode, DomainFault, records::*, results::OperationResult},
    services::sessions::{Failure, Refusal, decided, query_fault, rev},
};
struct PreparedRefusal(Refusal);
fn unsupported() -> Failure {
    Refusal::new(
        DomainErrorCode::UnsupportedCapability,
        "convention_operation",
        None,
    )
    .into()
}
impl ConventionService {
    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
    fn prepare_read(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::ReadInput,
        exact_root: bool,
    ) -> Result<PreparedDocument, Failure> {
        let (owner, resource) = env.read(|read| {
            Ok((|| {
                let owner = if let Some(id) = input.convention_id.as_ref() {
                    access::owner(
                        read,
                        ctx,
                        id,
                        input.revision.as_ref().map(|r| r.get()),
                        input.scope.as_ref(),
                    )?
                } else {
                    let slug = input.slug.as_ref().ok_or_else(unsupported)?;
                    let scope = input.scope.as_ref().ok_or_else(unsupported)?;
                    let row = self.resolve_slug(read, ctx, scope, &slug.0, exact_root)?;
                    if let Some(revision) = input.revision.as_ref() {
                        access::owner(read, ctx, &row.id, Some(revision.get()), Some(scope))?
                    } else {
                        row
                    }
                };
                let resource = if let Some(id) = input.resource_id.as_ref() {
                    let scoped = access::scope(read, ctx, input.scope.as_ref(), false)?;
                    Some(
                        read.conventions()
                            .convention_resource(
                                &owner.id,
                                id,
                                Some(owner.revision),
                                &scoped.readable,
                            )?
                            .ok_or_else(|| {
                                crate::services::sessions::not_found("convention_resource", id)
                            })?,
                    )
                } else {
                    None
                };
                Ok::<_, Failure>((owner, resource))
            })())
        })??;
        let mut intent =
            serde_json::to_value(input).map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        intent
            .as_object_mut()
            .expect("input object")
            .remove("cursor");
        let request = DocumentRequest {
            owner,
            resource,
            scope: input.scope.as_ref(),
            format: input.format.clone().unwrap_or(ConventionFormat::Markdown),
            cursor: input.cursor.as_ref().map(|c| c.0.as_str()),
            max_chars: input.max_chars.as_ref().map_or(16384, |n| n.get() as usize),
            max_bytes: input.max_bytes.as_ref().map_or(49152, |n| n.get() as usize),
            intent,
        };
        match request.format {
            ConventionFormat::SectionPath => self.prepare_path(
                ctx,
                env,
                request,
                input.section_path.as_ref().ok_or_else(unsupported)?,
            ),
            ConventionFormat::Manifest => self.prepare_manifest(ctx, env, request),
            _ => self.prepare_document(ctx, env, request),
        }
    }
}
fn resource_document(input: &ops::ResourcesInput) -> Option<ops::ReadInput> {
    let Some(ConventionResourcesSelection::Document {
        root_slug,
        cursor,
        max_chars,
    }) = input.selection.as_ref()
    else {
        return None;
    };
    Some(ops::ReadInput {
        convention_id: None,
        revision: None,
        slug: Some(root_slug.clone()),
        scope: Some(input.scope.clone()),
        format: Some(ConventionFormat::Markdown),
        cursor: cursor.clone(),
        max_chars: max_chars.clone(),
        section_path: None,
        operation_id: None,
        resource_id: None,
        max_bytes: None,
    })
}
impl QueryHandler for ConventionService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let result: Result<Option<Prepared>, Failure> = match query.input() {
            OperationInput::Conventions(ops::Input::Read(input))
                if input.operation_id.is_none() =>
            {
                self.prepare_read(ctx, env, input, false)
                    .map(|p| Some(Prepared::new(p)))
            }
            OperationInput::Conventions(ops::Input::Resources(input)) => {
                if let Some(document) = resource_document(input) {
                    self.prepare_read(ctx, env, &document, true)
                        .map(|p| Some(Prepared::new(p)))
                } else {
                    Ok(None)
                }
            }
            OperationInput::Conventions(ops::Input::Search(input)) => self
                .index
                .prepare_search(ctx, query, env, input)
                .map(|p| Some(Prepared::new(p))),
            _ => Ok(None),
        };
        result.map_err(|e| query_fault(e, "convention", self.correlation()))
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result: Result<(OperationResult, i64), Failure> = (|| match query.input() {
            OperationInput::Conventions(ops::Input::List(input)) => {
                let page = self.catalog_page(read, ctx, input, "conventions.list")?;
                let revision = page.scope_revision.get();
                Ok((
                    OperationResult::ConventionsList {
                        value: ConventionPageResult::Current(page),
                    },
                    revision,
                ))
            }
            OperationInput::Conventions(ops::Input::Version(input)) => {
                let page = self.versions(read, ctx, input)?;
                let revision = page.scope_revision.get();
                Ok((
                    OperationResult::ConventionsVersion {
                        value: ConventionPageResult::Current(page),
                    },
                    revision,
                ))
            }
            OperationInput::Conventions(ops::Input::Read(input)) => {
                if let Some(id) = input.operation_id.as_ref() {
                    let status = if read
                        .operations()
                        .operation(id)?
                        .is_some_and(|op| op.capability == "conventions.import")
                    {
                        self.imports.status(read, ctx, id, input.scope.as_ref())?
                    } else {
                        self.index_operation(read, ctx, id, input.scope.as_ref())?
                    };
                    let revision = status.revision.get();
                    return Ok((
                        OperationResult::ConventionsRead {
                            value: ConventionReadResult::Current(
                                ConventionReadOutcome::Operation { status },
                            ),
                        },
                        revision,
                    ));
                }
                let prepared = query
                    .prepared::<PreparedDocument>()
                    .ok_or_else(unsupported)?;
                Ok((
                    OperationResult::ConventionsRead {
                        value: ConventionReadResult::Current(self.publish_document(
                            read,
                            ctx,
                            input.scope.as_ref(),
                            prepared,
                        )?),
                    },
                    prepared.scope_revision,
                ))
            }
            OperationInput::Conventions(ops::Input::Resources(input)) => {
                let (view, revision) = match input.selection.as_ref() {
                    Some(ConventionResourcesSelection::Attachments {
                        convention_id,
                        convention_revision,
                        lifecycles,
                    }) => {
                        let page = self.attachments(
                            read,
                            ctx,
                            input,
                            convention_id,
                            convention_revision.as_ref(),
                            lifecycles.as_ref(),
                        )?;
                        let revision = page.scope_revision.get();
                        (ConventionResourcesView::Attachments { page }, revision)
                    }
                    Some(ConventionResourcesSelection::Document { .. }) => {
                        let prepared = query
                            .prepared::<PreparedDocument>()
                            .ok_or_else(unsupported)?;
                        let ConventionReadOutcome::Document { view } =
                            self.publish_document(read, ctx, Some(&input.scope), prepared)?
                        else {
                            return Err(unsupported());
                        };
                        (
                            ConventionResourcesView::Document { view },
                            prepared.scope_revision,
                        )
                    }
                    _ => {
                        let list = ops::ListInput {
                            scope: input.scope.clone(),
                            page: input.page.clone(),
                            applicability: input.applicability.clone(),
                            lifecycles: None,
                            order: Some(crate::protocol::records::ConventionOrder::Title),
                        };
                        let page = self.catalog_page(read, ctx, &list, "conventions.resources")?;
                        let revision = page.scope_revision.get();
                        (ConventionResourcesView::Catalog { page }, revision)
                    }
                };
                Ok((
                    OperationResult::ConventionsResources {
                        value: ConventionResourcePageResult::Current(view),
                    },
                    revision,
                ))
            }
            OperationInput::Conventions(ops::Input::Search(input)) => {
                let prepared = query
                    .prepared::<super::indexing::query::PreparedSearch>()
                    .ok_or_else(unsupported)?;
                let page = self.index.search(read, ctx, query, input, prepared)?;
                let revision = page.scope_revision.get();
                Ok((
                    OperationResult::ConventionsSearch {
                        value: ConventionSearchPageResult::Current(page),
                    },
                    revision,
                ))
            }
            _ => Err(unsupported()),
        })();
        let (result, revision) =
            result.map_err(|e| query_fault(e, "convention", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: read
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}
impl CommandHandler for ConventionService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let result = match command.input() {
            OperationInput::Conventions(ops::Input::Edit(input)) => self
                .prepare_edit(ctx, env, input)
                .map(|p| Some(Prepared::new(p))),
            OperationInput::Conventions(ops::Input::Move(input)) => self
                .prepare_move(ctx, env, input)
                .map(|p| Some(Prepared::new(p))),
            OperationInput::Conventions(ops::Input::Import(input)) => self
                .imports
                .prepare_import(ctx, input, env)
                .map(|p| Some(Prepared::new(p))),
            _ => Ok(None),
        };
        match result {
            Ok(p) => Ok(p),
            Err(Failure::Storage(error)) => Err(error),
            Err(Failure::Refused(refusal)) => Ok(Some(Prepared::new(PreparedRefusal(refusal)))),
        }
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if self.is_stopped() {
            return Err(StorageError::Cancelled.into());
        }
        if let Some(refusal) = command.prepared::<PreparedRefusal>() {
            return decided(tx, Err(refusal.0.clone().into()));
        }
        let result: Result<Decided, Failure> = (|| match command.input() {
            OperationInput::Conventions(ops::Input::Import(input)) => {
                self.imports.decide_import(ctx, command, tx, input)
            }
            OperationInput::Conventions(ops::Input::Index(input)) => {
                self.index.decide_index(ctx, command, tx, input)
            }
            OperationInput::Conventions(ops::Input::Retire(input))
                if input.operation_id.is_some() =>
            {
                let id = input.operation_id.as_ref().expect("matched");
                if tx
                    .operations()
                    .operation(id)?
                    .is_some_and(|op| op.capability == "conventions.import")
                {
                    self.imports
                        .cancel(tx, ctx, id, &input.expected_revision, input.scope.as_ref())
                } else {
                    self.cancel_index(tx, ctx, id, &input.expected_revision, input.scope.as_ref())
                }
            }
            OperationInput::Conventions(input) => {
                let decision = match input {
                    ops::Input::Edit(input) => self.edit(ctx, command, tx, input),
                    ops::Input::Move(input) => self.move_tree(ctx, command, tx, input),
                    ops::Input::Applicability(input) => self.applicability(ctx, tx, input),
                    ops::Input::Retire(input) => self.retire(ctx, tx, input),
                    ops::Input::Delete(input) => self.delete(ctx, tx, input),
                    ops::Input::DeleteResource(input) => self.delete_resource(ctx, tx, input),
                    _ => Err(unsupported()),
                }?;
                Ok(Decided {
                    decision,
                    operation_id: None,
                })
            }
            _ => Err(unsupported()),
        })();
        match result {
            Ok(value) => Ok(value),
            Err(error) => decided(tx, Err(error)),
        }
    }
}
