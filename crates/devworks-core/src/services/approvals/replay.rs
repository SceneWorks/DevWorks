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
    json!({"principal":c.principal(),"kind":format!("{:?}",c.principal_kind()),"invocation":c.invocation_id(),"session":c.session_id(),"jobRun":c.job_run_id(),"namespace":c.ingress_namespace(),"project":c.project_id(),"root":c.root_id(),"assignment":c.assignment_id(),"capability":c.capability_registration(),"policyRevision":c.policy_revision(),"worktree":c.worktree().map(|w|json!({"id":w.id(),"repository":w.repository_id(),"work":w.work_id(),"revision":w.ownership_revision()}))})
}
fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, StorageError> {
    serde_json::from_value(v).map_err(|e| StorageError::InvalidRecord(e.to_string()))
}
/// Private source provenance, independent of an approval or any authorization token.
/// A source without this versioned captured context is not automatically replayable.
pub(crate) fn save_source_context(c: &InvocationContext) -> Value {
    json!({"version":1,"storeId":c.store_id(),"restoreEpoch":c.restore_epoch(),"invocationContext":save_context(c)})
}
pub(crate) fn restore_source_context(
    store: &StoreInfo,
    principal: &crate::protocol::ActorId,
    kind: PrincipalKind,
    source: &Value,
) -> Result<InvocationContext, StorageError> {
    if source["version"].as_u64() != Some(1)
        || source["storeId"] != json!(store.store_id)
        || parse::<Counter>(source["restoreEpoch"].clone())?.get() != store.restore_epoch
    {
        return Err(StorageError::InvalidRecord(
            "source context store/version/restore binding".into(),
        ));
    }
    invocation_context(
        &store.store_id,
        store.restore_epoch,
        principal,
        kind,
        &source["invocationContext"],
    )
}
fn invocation_context(
    store_id: &StoreId,
    restore_epoch: i64,
    principal: &crate::protocol::ActorId,
    kind: PrincipalKind,
    v: &Value,
) -> Result<InvocationContext, StorageError> {
    if !v.is_object()
        || v["principal"] != json!(principal)
        || v["kind"] != json!(format!("{kind:?}"))
    {
        return Err(StorageError::InvalidRecord(
            "source context principal binding".into(),
        ));
    }
    let mut c = InvocationContext::new(
        principal.clone(),
        kind,
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
    if let Some(session) = parse::<Option<Id>>(v["session"].clone())? {
        c = c.with_session(session);
    }
    // Legacy captures omit this field. Never infer a run from an invocation or namespace.
    if let Some(run) = parse::<Option<Id>>(v["jobRun"].clone())? {
        c = c.with_job_run(run);
    }
    if let Some(capability) = parse::<Option<Id>>(v["capability"].clone())? {
        c = c.with_capability_registration(capability);
    }

    if !v["worktree"].is_null() {
        let w = &v["worktree"];
        c = c.bind_worktree(crate::context::BoundWorktree::resolved(
            parse(w["id"].clone())?,
            parse(w["repository"].clone())?,
            parse(w["work"].clone())?,
            w["revision"]
                .as_i64()
                .filter(|n| *n > 0)
                .ok_or_else(|| StorageError::InvalidRecord("ownership revision".into()))?,
        ));
    }
    Ok(c)
}
fn base_context(
    store: &StoreInfo,
    p: &StoredApproval,
    grant: Option<&Id>,
) -> Result<InvocationContext, StorageError> {
    // Older generic proposals already captured the exact invocation privately. Their source
    // restore epoch and containing store supply the same envelope without inventing lineage.
    let legacy;
    let source = if let Some(source) = p.payload.get("sourceContext") {
        source
    } else {
        legacy = json!({"version":1,"storeId":store.store_id,"restoreEpoch":p.restore_epoch.to_string(),"invocationContext":p.payload["invocation"]});
        &legacy
    };
    let mut c = super::restore_source_context(store, &p.author, p.author_kind, source)?;
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
    store: &StoreInfo,
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
    base_context(store, p, grant)
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
    Ok(base_context(tx.store(), p, grant)?)
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
        let actual = crate::ports::AuthorityRequest {
            descriptor: request.descriptor,
            binding: request.binding,
            effect: request.effect,
            action: request.action,
            input: &request.input,
            fingerprint: &request.fingerprint,
        };
        let kind = crate::authority::service::reviewed_grant_kind(tx, &original, &actual)
            .map_err(|_| denied("reviewed_operation_current_authority"))?;
        let now = tx.now_ms();
        let g = crate::authority::service::mint_reviewed_grant(
            tx,
            human,
            &p.id,
            p.author.clone(),
            kind,
            crate::authority::service::GrantBinding {
                operation: p.operation_kind.clone(),
                action: request.action.map(str::to_owned),
                request_digest: Some(p.content_digest.clone()),
                project_id: original.project_id().cloned(),
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
            let ctx = base_context(&store.info(), &p, r.grant_id.as_ref())?;
            if s.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                || ctx.restore_epoch().get() != p.restore_epoch
            {
                return Err(StorageError::InvalidRecord("stale proposer".into()));
            }
            let identity =
                crate::context::ReceiptIdentity::bind(&ctx, RequestKey(r.request_key.clone()));
            let replay = s.receipts().lookup(&identity)?.is_some();
            if !replay && let Some(root) = ctx.root_id() {
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
        let original = request.context.clone();
        let operation = request.operation.clone();
        let decoded = crate::admission::decode(
            crate::admission::Selection {
                operation: &operation,
                mode: crate::registry::Mode::Command,
            },
            &request.frame,
        )
        .map_err(|_| StorageError::InvalidRecord("resume settlement frame".into()))?;
        let crate::admission::CallerKey::Command(request_key) = decoded.key else {
            return Err(StorageError::InvalidRecord("resume settlement key".into()));
        };
        let fingerprint = decoded.fingerprint;
        let schema_revision = decoded.revision;

        let value = outcome
            .as_ref()
            .ok()
            .map(|r| serde_json::to_value(r).unwrap());
        let state = match outcome {
            Ok(CommandReply::ReceiptPurged { outcome, .. }) => {
                if matches!(
                    outcome.original_decision,
                    crate::protocol::records::PurgedDecisionKind::Refused
                ) {
                    "Refused"
                } else {
                    "Admitted"
                }
            }
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
        let needs_purged_lookup = matches!(
            outcome,
            Ok(CommandReply::Rejected {
                code: crate::protocol::Rejection::UnsupportedSchema,
                retry: crate::protocol::RetryAdvice::ResolveReceipt,
                ..
            })
        );
        run_write(store, WriteOptions::default(), move |tx| {
            let Some(mut r) = tx.decisions().approval_resume(&id)? else {
                return Ok(());
            };
            if r.revision != expected || !matches!(r.state.as_str(), "Pending" | "Unknown") {
                return Ok(());
            }
            if request_key.0 != r.request_key
                || original.store_id() != &tx.store().store_id
                || original.restore_epoch().get() != tx.store().restore_epoch
            {
                return Err(StorageError::InvalidRecord(
                    "resume settlement identity".into(),
                ));
            }
            if r.principal != *original.principal()
                || r.principal_kind != original.principal_kind()
                || r.invocation != save_context(&original)
                || r.operation != operation
                || r.request_digest != fingerprint.as_str()
                || r.schema_revision != i64::from(schema_revision.0)
            {
                return Err(StorageError::InvalidRecord(
                    "resume settlement binding".into(),
                ));
            }
            let identity =
                crate::context::ReceiptIdentity::bind(&original, RequestKey(r.request_key.clone()));

            let durable = if state == "Refused" || needs_purged_lookup {
                tx.receipts().lookup(&identity)?
            } else {
                None
            };
            let (state, value) = match durable {
                Some(LedgerEntry::Purged(stored))
                    if stored.operation == operation
                        && stored.contract == crate::protocol::CONTRACT
                        && stored.schema_revision == schema_revision
                        && stored.fingerprint == fingerprint =>
                {
                    let state = if matches!(
                        stored.outcome.original_decision,
                        crate::protocol::records::PurgedDecisionKind::Refused
                    ) {
                        "Refused"
                    } else {
                        "Admitted"
                    };
                    (
                        state.to_owned(),
                        Some(
                            serde_json::to_value(CommandReply::<DomainDecision>::ReceiptPurged {
                                protocol: crate::protocol::Protocol::Draft1,
                                request_key: request_key.clone(),
                                outcome: stored.outcome,
                            })
                            .expect("purged receipt identity"),
                        ),
                    )
                }
                Some(LedgerEntry::Current(stored))
                    if stored.operation == operation
                        && stored.contract == crate::protocol::CONTRACT
                        && stored.schema_revision == schema_revision
                        && stored.fingerprint == fingerprint =>
                {
                    let state = if matches!(stored.receipt.decision, DomainDecision::Refused { .. })
                    {
                        "Refused"
                    } else {
                        "Admitted"
                    };
                    (
                        state.to_owned(),
                        Some(
                            serde_json::to_value(CommandReply::Recorded {
                                protocol: crate::protocol::Protocol::Draft1,
                                receipt: stored.receipt,
                                replayed: true,
                            })
                            .expect("durable receipt"),
                        ),
                    )
                }
                _ if r.state == "Unknown" && state == "Refused" => ("Unknown".into(), value),
                _ => (state, value),
            };
            super::held::settle(
                tx,
                &id,
                &state,
                json!({"approvalId": id, "decision": "Approved", "result": value}),
            )?;
            r.revision += 1;
            r.state = state;
            r.updated_at_ms = tx.now_ms();
            r.result = value;
            tx.decisions().put_approval_resume(&r, Some(expected))
        })
    }
}

#[cfg(test)]
mod source_context_tests {
    use super::*;
    use crate::protocol::{ActorId, AssignmentId, ProjectId, Revision, RootId};
    fn id(s: &str) -> Id {
        Id::new(s).unwrap()
    }
    fn store() -> StoreInfo {
        StoreInfo {
            store_id: StoreId(id("source-store")),
            restore_epoch: 7,
            boot_id: None,
            host_actor: ActorId(id("host")),
            access: AccessMode::ReadWrite,
        }
    }
    fn original() -> InvocationContext {
        InvocationContext::new(
            ActorId(id("original-agent")),
            PrincipalKind::Agent,
            id("actual-invocation"),
            id("actual-namespace"),
            store().store_id,
            Counter::new(7).unwrap(),
            Revision::new(11).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("root"))),
            Some(AssignmentId(id("assignment"))),
        )
        .with_session(id("actual-session"))
        .with_job_run(id("actual-run"))
        .with_capability_registration(id("actual-tool"))
        .with_authority_source(id("old-grant"))
        .bind_worktree(crate::context::BoundWorktree::resolved(
            id("tree"),
            id("repository"),
            id("work"),
            3,
        ))
    }
    #[test]
    fn captured_source_context_preserves_exact_provenance_without_minting_consent() {
        let original = original();
        let captured = save_source_context(&original);
        let restored = restore_source_context(
            &store(),
            original.principal(),
            PrincipalKind::Agent,
            &captured,
        )
        .unwrap();
        assert_eq!(save_context(&original), save_context(&restored));
        assert_eq!(restored.principal_kind(), PrincipalKind::Agent);
        assert!(restored.reviewed_operation().is_none());
        assert!(restored.authority_sources().is_empty());
        assert_eq!(restored.job_run_id(), Some(&id("actual-run")));
        let mut legacy = captured.clone();
        legacy["invocationContext"]
            .as_object_mut()
            .unwrap()
            .remove("jobRun");
        assert!(
            restore_source_context(
                &store(),
                original.principal(),
                PrincipalKind::Agent,
                &legacy
            )
            .unwrap()
            .job_run_id()
            .is_none()
        );
        assert!(
            restore_source_context(
                &store(),
                original.principal(),
                PrincipalKind::Human,
                &captured
            )
            .is_err()
        );
        assert!(
            restore_source_context(
                &store(),
                &ActorId(id("other-agent")),
                PrincipalKind::Agent,
                &captured
            )
            .is_err()
        );
        for (field, value) in [
            ("storeId", json!("other-store")),
            ("restoreEpoch", json!("8")),
            ("version", json!(2)),
        ] {
            let mut changed = captured.clone();
            changed[field] = value;
            assert!(
                restore_source_context(
                    &store(),
                    original.principal(),
                    PrincipalKind::Agent,
                    &changed
                )
                .is_err()
            );
        }
        for field in ["invocation", "namespace", "policyRevision"] {
            let mut changed = captured.clone();
            changed["invocationContext"][field] = Value::Null;
            assert!(
                restore_source_context(
                    &store(),
                    original.principal(),
                    PrincipalKind::Agent,
                    &changed
                )
                .is_err()
            );
        }
        let mut changed = captured;
        changed["invocationContext"]["session"] = json!(42);
        assert!(
            restore_source_context(
                &store(),
                original.principal(),
                PrincipalKind::Agent,
                &changed
            )
            .is_err()
        );
        assert!(
            restore_source_context(
                &store(),
                original.principal(),
                PrincipalKind::Agent,
                &save_context(&original)
            )
            .is_err(),
            "legacy unknown source context is not synthesized"
        );
    }
}
