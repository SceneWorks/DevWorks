use super::{OwnerPreview, SavedOperationOwners, model, requests};
use crate::admission::DecodedRequest;
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, approvals as ops};
use crate::ports::storage::*;
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::records::{ApprovalContent, ApprovalViewRecord};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id};
use crate::services::{
    UseCases,
    cursor::CursorBinding,
    sessions::{Failure, Refusal, not_found, project_visible, query_fault, rev},
};
use crate::settings::service::PrincipalRoles;
use serde_json::Value;
use std::sync::Arc;
pub const OPERATIONS: [&str; 5] = [
    "approvals.list",
    "approvals.inspect",
    "approvals.propose",
    "approvals.record_human_decision",
    "approvals.review_queue",
];
#[derive(Clone, Debug, Default)]
pub struct ApprovalCatalog {
    operations: Vec<&'static str>,
}
impl ApprovalCatalog {
    pub fn qualified(cases: &UseCases) -> Self {
        Self {
            operations: crate::registry::all()
                .iter()
                .filter(|d| requests::candidate(d.name) && cases.has_command(d.name))
                .map(|d| d.name)
                .collect(),
        }
    }
    pub fn contains(&self, op: &str) -> bool {
        self.operations.contains(&op)
    }
}
struct NoOwners;
impl SavedOperationOwners for NoOwners {
    fn supports(&self, _: &str) -> bool {
        false
    }
    fn prepare(
        &self,
        _: &InvocationContext,
        _: &DecodedRequest,
        _: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        Err(unsupported())
    }
    fn validate(
        &self,
        _: &mut dyn WriteTransaction,
        _: &InvocationContext,
        _: &DecodedRequest,
        _: &OwnerPreview,
    ) -> Result<(), Failure> {
        Err(unsupported())
    }
}
pub struct ApprovalService {
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) roles: Arc<dyn PrincipalRoles>,
    pub(crate) control: Arc<crate::services::runs::ControlService>,
    pub(crate) catalog: ApprovalCatalog,
    pub(crate) owners: Arc<dyn SavedOperationOwners>,
}
impl ApprovalService {
    pub fn new(
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        roles: Arc<dyn PrincipalRoles>,
        control: Arc<crate::services::runs::ControlService>,
    ) -> Self {
        Self {
            ids,
            clock,
            roles,
            control,
            catalog: ApprovalCatalog::default(),
            owners: Arc::new(NoOwners),
        }
    }
    pub fn with_catalog(mut self, catalog: ApprovalCatalog) -> Self {
        self.catalog = catalog;
        self
    }
    pub fn with_owners(mut self, owners: Arc<dyn SavedOperationOwners>) -> Self {
        self.owners = owners;
        self
    }
    pub fn publish_pending_notifications(
        &self,
        tx: &mut dyn WriteTransaction,
        after: Option<&ApprovalSourcePosition>,
        limit: u16,
    ) -> Result<Option<ApprovalSourcePosition>, StorageError> {
        let rows = tx.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: Some("Pending".into()),
            },
            after,
            limit,
        )?;
        for r in &rows {
            crate::services::notifications::notify_approval(
                tx,
                r.source,
                &r.id,
                r.project_id.as_ref(),
                "Approval required",
                "Review the exact proposed changes before deciding.",
            )?;
        }
        Ok(if rows.len() == usize::from(limit) {
            rows.last().map(|r| ApprovalSourcePosition {
                created_at_ms: r.created_at_ms,
                id: r.id.clone(),
                source: r.source,
            })
        } else {
            None
        })
    }
    pub(crate) fn validate_owner(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        request: &DecodedRequest,
        preview: &OwnerPreview,
    ) -> Result<(), Failure> {
        check_authority(tx, ctx, request)?;
        if builtin(request.descriptor.name) {
            crate::services::projects::validate_approval_preview(tx, ctx, request, preview)
        } else {
            self.owners.validate(tx, ctx, request, preview)
        }
    }
    pub(crate) fn decoded(
        &self,
        op: &str,
        revision: u32,
        input: &Value,
    ) -> Result<DecodedRequest, Failure> {
        requests::decode(
            op,
            revision,
            input,
            self.catalog.contains(op) && (builtin(op) || self.owners.supports(op)),
        )
        .map_err(|e| match e {
            requests::RequestFailure::Unsupported => unsupported(),
            requests::RequestFailure::Invalid => invalid("saved_operation"),
        })
    }
    pub(crate) fn inspect(
        &self,
        s: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &Id,
        full: bool,
    ) -> Result<ApprovalViewRecord, Failure> {
        if let Some(p) = s.configuration().proposal(id)? {
            if p.namespace != "settings" || !project_visible(ctx, p.scope.project_id()) {
                return Err(not_found("approval", id).into());
            }
            let n = s
                .configuration()
                .current(&p.scope, "settings")?
                .map_or(1, |c| c.revision);
            let mut view = model::settings(&p, n, full)?;
            view.rationale = s
                .decisions()
                .human_decision(ApprovalSource::Settings, id)?
                .and_then(|a| a.rationale)
                .map(|v| crate::services::sessions::content(&v));
            return Ok(view);
        }
        if let Some(p) = s.work().proposal(id)? {
            let f = s
                .work()
                .work(&p.feature_id)?
                .filter(|f| project_visible(ctx, Some(&f.project_id)))
                .ok_or_else(|| not_found("approval", id))?;
            let mut view = model::plan(
                &p,
                &f.project_id,
                s.work().feature_plan(&f.id)?.as_ref(),
                full,
            )?;
            view.rationale = s
                .decisions()
                .human_decision(ApprovalSource::FeaturePlan, id)?
                .and_then(|a| a.rationale)
                .map(|v| crate::services::sessions::content(&v));
            return Ok(view);
        }
        let p = s
            .decisions()
            .approval(id)?
            .filter(|p| project_visible(ctx, p.scope.project_id()))
            .ok_or_else(|| not_found("approval", id))?;
        let mut view = model::generic(&p, full, s.decisions().approval_resume(id)?.as_ref())?;
        if p.state == "Pending" && p.payload["content"]["kind"] == "FindingRisk" {
            let feature: Id = serde_json::from_value(p.payload["content"]["featureId"].clone())
                .map_err(|_| invalid("finding_risk"))?;
            view.stale =
                Some(s.work().readiness(&feature)?.is_none_or(|r| {
                    model::hash(&r.body) != p.payload["binding"]["assessmentDigest"]
                }));
        }
        Ok(view)
    }
    fn list(
        &self,
        s: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::ListInput,
        revision: u32,
    ) -> Result<OperationResult, Failure> {
        let scope = scope(ctx, &i.scope)?;
        let state = i.state.as_ref().map(|v| {
            serde_json::to_value(v)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        });
        let filter = ApprovalSourceFilter { scope, state };
        let bind = CursorBinding::new(
            ctx,
            "approvals.list",
            &[
                &serde_json::to_string(&i.scope).unwrap(),
                &serde_json::to_string(&i.state).unwrap(),
                &revision.to_string(),
            ],
            1,
        );
        let after = i
            .page
            .cursor
            .as_ref()
            .map(|c| bind.decode(&c.0))
            .transpose()?
            .map(|p| parse_position(&p))
            .transpose()?;
        let limit = i.page.limit.get() as u16;
        let refs = s
            .decisions()
            .list_approval_sources(&filter, after.as_ref(), limit)?;
        let more = refs.len() == usize::from(limit);
        let next = if more {
            refs.last().map(|p| {
                BoundedText(bind.encode(&format!(
                    "{}|{}|{}",
                    p.created_at_ms,
                    p.id.as_str(),
                    p.source.as_str()
                )))
            })
        } else {
            None
        };
        let mut rows = Vec::with_capacity(refs.len());
        for r in refs {
            rows.push(self.inspect(s, ctx, &r.id, false)?)
        }
        Ok(OperationResult::ApprovalsList {
            value: Page {
                items: BoundedList(rows),
                next_cursor: next,
                has_more: more,
            },
        })
    }
}
pub(crate) fn scope(ctx: &InvocationContext, s: &DataScope) -> Result<ConfigScope, Failure> {
    let s = match s {
        DataScope::Workspace => ConfigScope::Workspace,
        DataScope::Project { project_id } => ConfigScope::Project(project_id.clone()),
    };
    if !project_visible(ctx, s.project_id()) {
        return Err(denied("approval_scope"));
    }
    Ok(s)
}
pub(crate) fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}
pub(crate) fn denied(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::PermissionDenied, kind, None).into()
}
pub(crate) fn unsupported() -> Failure {
    Refusal::new(
        DomainErrorCode::UnsupportedCapability,
        "approval_operation",
        None,
    )
    .into()
}
pub(crate) fn conflict(id: &Id) -> Failure {
    Refusal::new(DomainErrorCode::SourceChanged, "approval", Some(id.clone()))
        .retry(crate::protocol::RetryAdvice::RefreshAndNewRequest)
        .into()
}
fn parse_position(s: &str) -> Result<ApprovalSourcePosition, StorageError> {
    let p: Vec<_> = s.split('|').collect();
    if p.len() != 3 {
        return Err(StorageError::CursorExpired);
    }
    Ok(ApprovalSourcePosition {
        created_at_ms: p[0].parse().map_err(|_| StorageError::CursorExpired)?,
        id: Id::new(p[1]).map_err(|_| StorageError::CursorExpired)?,
        source: ApprovalSource::parse(p[2]).ok_or(StorageError::CursorExpired)?,
    })
}
impl QueryHandler for ApprovalService {
    fn query(
        &self,
        ctx: &InvocationContext,
        q: &AdmittedQuery,
        s: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match q.input() {
            OperationInput::Approvals(ops::Input::List(i)) => {
                self.list(s, ctx, i, q.schema_revision().0)
            }
            OperationInput::Approvals(ops::Input::Inspect(i)) => self
                .inspect(s, ctx, &i.proposal_id, true)
                .map(|value| OperationResult::ApprovalsInspect { value }),
            OperationInput::Approvals(ops::Input::ReviewQueue(i)) => {
                super::review_queue::query(s, ctx, i, q.schema_revision().0)
            }
            _ => Err(unsupported()),
        }
        .map_err(|f| {
            query_fault(
                f,
                "approval",
                CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: s.stamp().read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}
#[derive(Clone)]
pub(crate) struct PreparedOwner {
    pub preview: OwnerPreview,
}
/// A business owner can refuse preparation without publishing a false reviewable proposal.
/// Preserve its typed reason through the command decision instead of erasing it as metadata.
#[derive(Clone)]
struct PreparedOwnerRefusal(Refusal);

impl CommandHandler for ApprovalService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        if let OperationInput::Approvals(ops::Input::Propose(i)) = c.input()
            && scope(ctx, &i.scope).is_err()
        {
            return Ok(None);
        }
        if let OperationInput::Approvals(ops::Input::RecordHumanDecision(i)) = c.input()
            && matches!(
                i.decision,
                crate::protocol::records::ProposalDecision::Reject
            )
        {
            return Ok(None);
        }
        let candidate = match c.input() {
            OperationInput::Approvals(ops::Input::Propose(i)) => match &i.payload {
                Some(ApprovalContent::SavedOperation {
                    schema_revision,
                    input,
                }) => Some((
                    i.operation_kind.0.clone(),
                    schema_revision.0.parse::<u32>().unwrap_or(0),
                    input.0.clone(),
                    ctx.clone(),
                )),
                _ => None,
            },
            OperationInput::Approvals(ops::Input::RecordHumanDecision(i)) => env.read(|s| {
                let p = s.decisions().approval(&i.proposal_id)?;
                Ok(p.and_then(|p| {
                    if p.state != "Pending" {
                        return None;
                    }
                    let c = p.payload.get("content")?;
                    if c["kind"] != "SavedOperation" {
                        return None;
                    }
                    let original = super::replay::restore_context(&env.info(), s, &p, None).ok()?;
                    Some((
                        p.operation_kind.clone(),
                        c["schemaRevision"]
                            .as_str()
                            .and_then(|v| v.parse().ok())
                            .or_else(|| c["schemaRevision"].as_u64().map(|v| v as u32))
                            .unwrap_or(0),
                        c["input"].clone(),
                        original,
                    ))
                }))
            })?,
            _ => None,
        };
        let Some((op, revision, input, original)) = candidate else {
            return Ok(None);
        };
        let request = match self.decoded(&op, revision, &input) {
            Ok(r) => r,
            Err(_) => return Ok(None),
        };
        if !env.read(|s| super::prepare_authority(s, &original, &request))? {
            return Ok(None);
        }
        let prepared = if builtin(&op) {
            env.read(|s| {
                Ok(crate::services::projects::approval_preview(
                    s.projects(),
                    &original,
                    &request,
                ))
            })
            .map_err(Failure::Storage)
            .and_then(|v| v)
        } else {
            self.owners.prepare(&original, &request, env)
        };
        match prepared {
            Ok(preview) => Ok(Some(Prepared::new(PreparedOwner { preview }))),
            Err(Failure::Storage(e)) => Err(e),
            Err(Failure::Refused(refusal)) => {
                Ok(Some(Prepared::new(PreparedOwnerRefusal(refusal))))
            }
        }
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if let Some(refusal) = c.prepared::<PreparedOwnerRefusal>() {
            return crate::services::sessions::decided(tx, Err(refusal.0.clone().into()));
        }
        let out = match c.input() {
            OperationInput::Approvals(ops::Input::Propose(i)) => self.propose(tx, ctx, c, i),
            OperationInput::Approvals(ops::Input::RecordHumanDecision(i)) => {
                self.record(tx, ctx, c, i)
            }
            _ => Err(unsupported()),
        };
        crate::services::sessions::decided(tx, out.map(|result| DomainDecision::Applied { result }))
    }
}

fn builtin(op: &str) -> bool {
    matches!(
        op,
        "projects.archive" | "projects.delete" | "repositories.remove_index"
    )
}

fn policy_permits(
    ctx: &InvocationContext,
    request: &DecodedRequest,
    kind: Option<crate::context::PrincipalKind>,
    principal: Option<&PrincipalBinding>,
    policy: &crate::authority::policy::EffectivePolicy,
) -> bool {
    let actual = crate::ports::AuthorityRequest {
        descriptor: request.descriptor,
        binding: request.binding,
        effect: request.effect,
        action: request.action,
        input: &request.input,
        fingerprint: &request.fingerprint,
    };
    kind == Some(ctx.principal_kind())
        && !matches!(
            crate::authority::policy::evaluate(ctx, principal, policy, &actual, &[]),
            crate::authority::policy::Verdict::Deny(_)
        )
}
pub(crate) fn validate_authority(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    request: &DecodedRequest,
) -> Result<bool, StorageError> {
    let kind = tx.actors().actor_kind(ctx.principal())?;
    let principal = match ctx.principal_kind() {
        crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host => None,
        _ => tx.authority().principal(ctx.principal())?,
    };
    let policy = crate::authority::policy::resolve_policy(tx.configuration(), ctx.project_id())?;
    Ok(policy_permits(
        ctx,
        request,
        kind,
        principal.as_ref(),
        &policy,
    ))
}
pub(crate) fn check_authority(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    request: &DecodedRequest,
) -> Result<(), Failure> {
    if !super::validate_authority(tx, ctx, request)? {
        return Err(denied("saved_operation_current_authority"));
    }
    Ok(())
}
pub(crate) fn prepare_authority(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    request: &DecodedRequest,
) -> Result<bool, StorageError> {
    let kind = read.actors().actor_kind(ctx.principal())?;
    let principal = match ctx.principal_kind() {
        crate::context::PrincipalKind::Human | crate::context::PrincipalKind::Host => None,
        _ => read.authority().principal(ctx.principal())?,
    };
    let policy = crate::authority::policy::resolve_policy(read.configuration(), ctx.project_id())?;
    Ok(policy_permits(
        ctx,
        request,
        kind,
        principal.as_ref(),
        &policy,
    ))
}
