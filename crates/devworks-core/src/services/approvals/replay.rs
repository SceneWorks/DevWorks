use super::service::{denied, invalid};
use super::{ApprovalService, model, requests};
use crate::context::{InvocationContext, PrincipalKind, ReviewedOperation};
use crate::gateway::{CommandOutcome, GatewayFailure};
use crate::ports::storage::*;
use crate::protocol::results::DomainDecision;
use crate::protocol::shared::CommandReply;
use crate::protocol::{Counter, Id, RequestKey, StoreId};
use crate::services::sessions::Failure;
use serde_json::{Value, json};
pub struct ApprovedCommand {
    pub context: InvocationContext,
    pub operation: String,
    pub frame: Vec<u8>,
    pub approval_id: Id,
    pub resume_revision: i64,
}
pub enum ResumePreparation {
    Terminal,
    Waiting { reason: String },
    Ready(ApprovedCommand),
}
pub(crate) fn save_context(c: &InvocationContext) -> Value {
    json!({"principal":c.principal(),"kind":format!("{:?}",c.principal_kind()),"invocation":c.invocation_id(),"session":c.session_id(),"namespace":c.ingress_namespace(),"project":c.project_id(),"root":c.root_id(),"assignment":c.assignment_id(),"capability":c.capability_registration(),"policyRevision":c.policy_revision(),"worktree":c.worktree().map(|w|json!({"id":w.id(),"repository":w.repository_id(),"work":w.work_id(),"revision":w.ownership_revision()}))})
}
fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, StorageError> {
    serde_json::from_value(v).map_err(|e| StorageError::InvalidRecord(e.to_string()))
}
fn base_context(
    store_id: &StoreId,
    restore_epoch: i64,
    p: &StoredApproval,
    grant: Option<&Id>,
) -> Result<InvocationContext, StorageError> {
    let v = &p.payload["invocation"];
    let mut c = InvocationContext::new(
        p.author.clone(),
        p.author_kind,
        parse(v["invocation"].clone())?,
        parse(v["namespace"].clone())?,
        store_id.clone(),
        Counter::new(restore_epoch)
            .map_err(|_| StorageError::InvalidRecord("restoreepoch".into()))?,
        parse(v["policyRevision"].clone())?,
    )
    .with_lineage(
        parse(v["project"].clone())?,
        parse(v["root"].clone())?,
        parse(v["assignment"].clone())?,
    );
    if let Some(s) = v["session"].as_str() {
        c = c.with_session(Id::new(s).map_err(|_| StorageError::InvalidRecord("session".into()))?)
    }
    if let Some(s) = v["capability"].as_str() {
        c = c.with_capability_registration(
            Id::new(s).map_err(|_| StorageError::InvalidRecord("capability".into()))?,
        )
    }
    if !v["worktree"].is_null() {
        let w = &v["worktree"];
        c = c.bind_worktree(crate::context::BoundWorktree::resolved(
            parse(w["id"].clone())?,
            parse(w["repository"].clone())?,
            parse(w["work"].clone())?,
            w["revision"]
                .as_i64()
                .ok_or_else(|| StorageError::InvalidRecord("ownership revision".into()))?,
        ));
    }
    if let Some(g) = grant {
        c = c.with_authority_source(g.clone());
        let content = &p.payload["content"];
        let schema = content["schemaRevision"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| content["schemaRevision"].as_u64().map(|n| n as u32))
            .unwrap_or(0);
        c = c.with_reviewed_operation(ReviewedOperation {
            approval_id: p.id.clone(),
            approver: p
                .decided_by
                .clone()
                .ok_or_else(|| StorageError::InvalidRecord("approver".into()))?,
            operation: p.operation_kind.clone(),
            schema_revision: schema,
            request_digest: p.content_digest.clone(),
            owner_binding: p.payload["binding"].clone(),
        });
    }
    Ok(c)
}
pub(crate) fn restore_context(
    s: &dyn ReadSnapshot,
    p: &StoredApproval,
    grant: Option<&Id>,
) -> Result<InvocationContext, StorageError> {
    if p.restore_epoch != s.stamp().restore_epoch
        || s.actors().actor_kind(&p.author)? != Some(p.author_kind)
    {
        return Err(StorageError::InvalidRecord(
            "approval principal changed".into(),
        ));
    }
    base_context(&s.stamp().store_id, s.stamp().restore_epoch, p, grant)
}
pub(crate) fn restore_context_tx(
    tx: &mut dyn WriteTransaction,
    p: &StoredApproval,
    grant: Option<&Id>,
) -> Result<InvocationContext, Failure> {
    if p.restore_epoch != tx.store().restore_epoch
        || tx.actors().actor_kind(&p.author)? != Some(p.author_kind)
    {
        return Err(denied("approval_principal_changed"));
    }
    Ok(base_context(
        &tx.store().store_id,
        tx.store().restore_epoch,
        p,
        grant,
    )?)
}
impl ApprovalService {
    pub(crate) fn create_resume(
        &self,
        tx: &mut dyn WriteTransaction,
        human: &InvocationContext,
        p: &StoredApproval,
    ) -> Result<(), Failure> {
        let c = &p.payload["content"];
        let revision = c["schemaRevision"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .or_else(|| c["schemaRevision"].as_u64().map(|n| n as u32))
            .ok_or_else(|| invalid("schema_revision"))?;
        let request = self.decoded(&p.operation_kind, revision, &c["input"])?;
        let original = restore_context_tx(tx, p, None)?;
        let policy =
            crate::authority::policy::resolve_policy(tx.configuration(), original.project_id())?
                .digest();
        let kind =
            if crate::authority::policy::NATIVE_EXECUTION.contains(&p.operation_kind.as_str()) {
                GrantKind::NativeExecution
            } else {
                GrantKind::Operation
            };
        let now = tx.now_ms();
        let g = crate::authority::service::mint_grant(
            tx,
            human,
            p.author.clone(),
            kind,
            crate::authority::service::GrantBinding {
                operation: p.operation_kind.clone(),
                action: request.action.map(str::to_owned),
                request_digest: Some(p.content_digest.clone()),
                project_id: original
                    .project_id()
                    .cloned()
                    .or_else(|| p.scope.project_id().cloned()),
                root_id: original.root_id().cloned(),
                policy_digest: Some(policy),
            },
            crate::authority::service::GrantBounds {
                expires_at_ms: now + crate::authority::service::MAX_GRANT_LIFETIME_MS,
                uses: 1,
            },
        )
        .map_err(|_| denied("reviewed_operation_grant"))?;
        let resume = StoredApprovalResume {
            approval_id: p.id.clone(),
            revision: 1,
            state: "Pending".into(),
            principal: p.author.clone(),
            principal_kind: p.author_kind,
            invocation: p.payload["invocation"].clone(),
            operation: p.operation_kind.clone(),
            schema_revision: i64::from(revision),
            input: c["input"].clone(),
            request_key: Id::new(format!(
                "resume-{}",
                &model::hash(&json!([p.id, p.content_digest, p.restore_epoch]))[..40]
            ))
            .expect("stable request"),
            request_digest: p.content_digest.clone(),
            grant_id: Some(g.id),
            created_at_ms: now,
            updated_at_ms: now,
            result: None,
        };
        tx.decisions().put_approval_resume(&resume, None)?;
        Ok(())
    }
    pub fn prepare_resume(
        &self,
        store: &dyn ApplicationStore,
        id: &Id,
    ) -> Result<ResumePreparation, StorageError> {
        read_snapshot(store, &ReadOptions::default(), |s| {
            let Some(r) = s.decisions().approval_resume(id)? else {
                return Ok(ResumePreparation::Terminal);
            };
            if !matches!(r.state.as_str(), "Pending" | "Unknown") {
                return Ok(ResumePreparation::Terminal);
            }
            let p = s
                .decisions()
                .approval(id)?
                .ok_or_else(|| StorageError::InvalidRecord("approval missing".into()))?;
            if p.state != "Approved"
                || p.content_digest != r.request_digest
                || r.principal != p.author
                || r.principal_kind != p.author_kind
            {
                return Err(StorageError::InvalidRecord("approval binding".into()));
            }
            let audit = s
                .decisions()
                .human_decision(ApprovalSource::Generic, id)?
                .ok_or_else(|| StorageError::InvalidRecord("human audit missing".into()))?;
            if audit.decision != "Approved"
                || audit.actor
                    != p.decided_by
                        .clone()
                        .ok_or_else(|| StorageError::InvalidRecord("decider missing".into()))?
                || s.actors().actor_kind(&audit.actor)? != Some(PrincipalKind::Human)
            {
                return Err(StorageError::InvalidRecord("human audit binding".into()));
            }
            let ctx = base_context(
                &store.info().store_id,
                store.info().restore_epoch,
                &p,
                r.grant_id.as_ref(),
            )?;
            if s.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                || ctx.restore_epoch().get() != p.restore_epoch
            {
                return Err(StorageError::InvalidRecord("stale proposer".into()));
            }
            let identity =
                crate::context::ReceiptIdentity::bind(&ctx, RequestKey(r.request_key.clone()));
            let replay = s.receipts().lookup(&identity)?.is_some();
            if !replay {
                if let Some(root) = ctx.root_id() {
                    let Some(w) = s.sessions().work_root(root)? else {
                        return Ok(ResumePreparation::Waiting {
                            reason: "work scope no longer exists".into(),
                        });
                    };
                    let target = ctx
                        .assignment_id()
                        .map_or(crate::services::runs::Target::Root, |a| {
                            crate::services::runs::Target::Assignment(a.clone())
                        });
                    let e = self.control.evaluate(
                        s.runs(),
                        &w,
                        &target,
                        ActivationPurpose::Implementation,
                    )?;
                    if !e.eligible {
                        return Ok(ResumePreparation::Waiting {
                            reason: e
                                .conditions
                                .first()
                                .map_or_else(|| "work held".into(), |c| c.reason.clone()),
                        });
                    }
                }
            }
            let request = self
                .decoded(&r.operation, r.schema_revision as u32, &r.input)
                .map_err(|_| StorageError::InvalidRecord("unregistered saved operation".into()))?;
            if request.fingerprint.as_str() != r.request_digest {
                return Err(StorageError::InvalidRecord("changed saved request".into()));
            }
            let frame = requests::frame(
                &r.operation,
                r.schema_revision as u32,
                &r.input,
                r.request_key.as_str(),
            )
            .map_err(|_| StorageError::InvalidRecord("saved envelope".into()))?;
            Ok(ResumePreparation::Ready(ApprovedCommand {
                context: ctx,
                operation: r.operation,
                frame: serde_json::to_vec(&frame).expect("frame"),
                approval_id: id.clone(),
                resume_revision: r.revision,
            }))
        })
    }
    pub fn settle_resume(
        &self,
        store: &dyn ApplicationStore,
        request: &ApprovedCommand,
        outcome: &CommandOutcome,
    ) -> Result<(), StorageError> {
        let id = request.approval_id.clone();
        let expected = request.resume_revision;
        let value = outcome
            .as_ref()
            .ok()
            .map(|r| serde_json::to_value(r).unwrap());
        let state = match outcome {
            Ok(CommandReply::Recorded { receipt, .. }) => {
                if matches!(receipt.decision, DomainDecision::Refused { .. }) {
                    "Refused"
                } else {
                    "Admitted"
                }
            }
            Ok(CommandReply::CommandStatusUnknown { .. }) => "Unknown",
            Ok(CommandReply::Rejected { retry, .. }) => {
                if matches!(
                    retry,
                    crate::protocol::RetryAdvice::SameCommand
                        | crate::protocol::RetryAdvice::ResolveReceipt
                ) {
                    "Unknown"
                } else {
                    "Refused"
                }
            }
            Err(GatewayFailure::NoUseCase(_)) => "Refused",
            Err(_) => "Unknown",
        }
        .to_owned();
        run_write(store, WriteOptions::default(), move |tx| {
            let Some(mut r) = tx.decisions().approval_resume(&id)? else {
                return Ok(());
            };
            if r.revision != expected || !matches!(r.state.as_str(), "Pending" | "Unknown") {
                return Ok(());
            }
            r.revision += 1;
            r.state = state;
            r.updated_at_ms = tx.now_ms();
            r.result = value;
            tx.decisions().put_approval_resume(&r, Some(expected))
        })
    }
}
