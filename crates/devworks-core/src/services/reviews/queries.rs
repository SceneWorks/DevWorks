//! Scoped review history with the same cursor and revision rules as other work views.
use super::{ReviewService, ReviewStateRecord, decode, not_implemented, visible_review};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedQuery;
use crate::kernel::project_visibility::project_visible;
use crate::operations::{OperationInput, reviews as ops};
use crate::ports::storage::{ReadSnapshot, StorageError};
use crate::ports::{IdKind, QueryAnswer, QueryHandler};
use crate::protocol::data::DataScope;
use crate::protocol::records::{ReviewFinding, ReviewRecord};
use crate::protocol::results::OperationResult;
use crate::protocol::support::Page;
use crate::protocol::{CorrelationId, DomainFault, Id, ProjectId};
use crate::services::cursor::{self, CursorBinding};
use crate::services::kernel::{Failure, not_found, query_fault, rev};

fn page_id(value: Option<&str>) -> Result<Option<Id>, StorageError> {
    value
        .map(|id| Id::new(id.to_owned()).map_err(|_| StorageError::CursorExpired))
        .transpose()
}

impl ReviewService {
    fn list(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::ListInput,
        schema_revision: u32,
    ) -> Result<(Page<ReviewRecord>, i64), Failure> {
        let root = input
            .root_id
            .as_ref()
            .map(|id| crate::services::work::visible_work(snapshot.work(), ctx, &id.0))
            .transpose()?;
        let project: Option<&ProjectId> = match &input.scope {
            DataScope::Project { project_id } => Some(project_id),
            DataScope::Workspace => ctx
                .project_id()
                .or_else(|| root.as_ref().map(|work| &work.project_id)),
        };
        let revision = if let Some(project) = project {
            if !project_visible(ctx, Some(project))
                || root
                    .as_ref()
                    .is_some_and(|work| &work.project_id != project)
            {
                return Err(not_found("project", &project.0).into());
            }
            crate::services::projects::visible_project(snapshot.projects(), ctx, project)?.revision
        } else {
            if !matches!(
                ctx.principal_kind(),
                PrincipalKind::Human | PrincipalKind::Host
            ) {
                return Err(StorageError::ScopeDenied.into());
            }
            1
        };
        let wire = schema_revision.to_string();
        let binding = CursorBinding::new(
            ctx,
            "reviews.list",
            &[
                project.map_or("", |project| project.0.as_str()),
                input.root_id.as_ref().map_or("", |root| root.0.as_str()),
                &wire,
            ],
            revision,
        );
        let page = cursor::page(
            &binding,
            &input.page,
            |after, limit| {
                let after = page_id(after)?;
                snapshot
                    .git()
                    .code_reviews(project, input.root_id.as_ref(), after.as_ref(), limit)
            },
            |review| review.id.as_str().to_owned(),
        )?;
        // Local review has no representation under revision 1. Preserve the cursor even
        // when a page contains only local records invisible to an earlier client.
        let states = page.try_into_page(|row| {
            let result: ReviewStateRecord = decode(&row.body)?;
            Ok::<_, Failure>(result.value)
        })?;
        let records = if schema_revision < super::REVISION {
            let mut earlier = states;
            earlier
                .items
                .0
                .retain(|record| record.local_review.is_none());
            earlier
        } else {
            states
        };
        Ok((records, revision))
    }

    fn findings(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::FindingsInput,
        schema_revision: u32,
    ) -> Result<(Page<ReviewFinding>, i64), Failure> {
        let (row, review) = visible_review(snapshot, ctx, &input.review_id)?;
        if schema_revision < super::REVISION && review.value.local_review.is_some() {
            return Err(not_found("review", &row.id).into());
        }
        let binding = CursorBinding::new(
            ctx,
            "reviews.findings",
            &[row.id.as_str(), &schema_revision.to_string()],
            row.revision,
        );
        let page = cursor::page(
            &binding,
            &input.page,
            |after, limit| {
                let after = page_id(after)?;
                snapshot.git().code_findings(&row.id, after.as_ref(), limit)
            },
            |finding| finding.id.as_str().to_owned(),
        )?;
        Ok((
            page.try_into_page(|finding| decode(&finding.body))?,
            row.revision,
        ))
    }
}

impl QueryHandler for ReviewService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let schema = query.schema_revision().0;
        let result = match query.input() {
            OperationInput::Reviews(ops::Input::List(input)) => self
                .list(snapshot, ctx, input, schema)
                .map(|(value, revision)| (OperationResult::ReviewsList { value }, revision)),
            OperationInput::Reviews(ops::Input::Inspect(input)) => {
                visible_review(snapshot, ctx, &input.review_id).and_then(|(row, review)| {
                    if schema < super::REVISION && review.value.local_review.is_some() {
                        return Err(not_found("review", &row.id).into());
                    }
                    Ok((
                        OperationResult::ReviewsInspect {
                            value: review.value,
                        },
                        row.revision,
                    ))
                })
            }
            OperationInput::Reviews(ops::Input::Findings(input)) => self
                .findings(snapshot, ctx, input, schema)
                .map(|(value, revision)| (OperationResult::ReviewsFindings { value }, revision)),
            OperationInput::Reviews(ops::Input::Readiness(input)) => self
                .readiness(snapshot, ctx, input)
                .map(|(value, revision)| (OperationResult::ReviewsReadiness { value }, revision)),
            _ => Err(not_implemented()),
        };
        let (result, revision) = result.map_err(|error| {
            query_fault(
                error,
                "review",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}
