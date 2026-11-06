//! Approval business behavior on the real durable store; no inference or external effects.
use super::{Harness, T0};
use crate::storage::sqlite::SqliteStore;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::ports::storage::*;
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::records::SettingAssignment;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{ActorId, CommandReply, Counter, DomainErrorCode, Id, Revision};
use devworks_core::services::{UseCases, approvals::ApprovalService, runs::ControlService};
use devworks_core::settings::{
    catalog::{AgentRole, descriptor},
    service::PrincipalRoles,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn actor(s: &str) -> ActorId {
    ActorId(id(s))
}
struct Roles;
impl PrincipalRoles for Roles {
    fn role(&self, _: &InvocationContext) -> Option<AgentRole> {
        Some(AgentRole::WorkspaceCoordinator)
    }
}
struct Allow;
impl AuthorityPort for Allow {
    fn authorize(
        &self,
        _: &InvocationContext,
        _: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        Ok(Authorization::direct())
    }
}
struct Fx {
    h: Harness,
    s: SqliteStore,
    cases: UseCases,
    n: AtomicU64,
}
impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let s = h.store();
        run_write(&s, WriteOptions::default(), |tx| {
            for (w, k) in [
                ("user", PrincipalKind::Human),
                ("agent", PrincipalKind::Agent),
            ] {
                tx.actors().register(&actor(w), k, w)?;
            }
            tx.decisions().bind_notification_user(&actor("user"))?;
            Ok(())
        })
        .unwrap();
        let svc = Arc::new(ApprovalService::new(
            h.ids.clone(),
            h.clock.clone(),
            Arc::new(Roles),
            Arc::new(ControlService::new(h.ids.clone())),
        ));
        Self {
            h,
            s,
            cases: UseCases::new().with_approvals(svc),
            n: AtomicU64::new(0),
        }
    }
    fn ctx(&self, who: &str) -> InvocationContext {
        let info = self.s.info();
        HostIngress::new(&self.s).context(
            actor(who),
            if who == "user" {
                PrincipalKind::Human
            } else {
                PrincipalKind::Agent
            },
            id("invoke"),
            id("main"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
    }
    fn gateway(&self) -> devworks_core::gateway::Gateway<'_> {
        devworks_core::gateway::Gateway {
            authority: &Allow,
            store: &self.s,
            ids: &*self.h.ids,
        }
    }
    fn command(&self, ctx: &InvocationContext, op: &str, mut body: Value) -> DomainDecision {
        body["kind"] = json!(op);
        let frame = json!({"protocol":"1.0-draft.1","mode":"Command","schemaRevision":3,"requestKey":format!("key-{}",self.n.fetch_add(1,Ordering::SeqCst)),"body":body});
        match self
            .cases
            .command(&self.gateway(), ctx, op, frame.to_string().as_bytes())
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => receipt.decision,
            r => panic!("unrecorded: {r:?}"),
        }
    }
    fn query(&self, op: &str, mut body: Value) -> OperationResult {
        body["kind"] = json!(op);
        let frame = json!({"protocol":"1.0-draft.1","mode":"Query","schemaRevision":3,"queryId":"query","body":body});
        match self
            .cases
            .query(
                &self.gateway(),
                &self.ctx("user"),
                op,
                frame.to_string().as_bytes(),
            )
            .unwrap()
        {
            DomainQueryReply::Result { result, .. } => result,
            r => panic!("query refused: {r:?}"),
        }
    }
    fn stage(&self) -> devworks_core::ports::storage::ProposalRecord {
        let context = self.ctx("agent");
        let d = descriptor("delivery.autoMerge").unwrap();
        let entries: Vec<SettingAssignment> = serde_json::from_value(
            json!([{"key":"delivery.autoMerge","value":{"schema":d.schema_ref(),"value":false}}]),
        )
        .unwrap();
        run_write(&self.s, WriteOptions::default(), move |tx| {
            devworks_core::settings::service::stage_proposal(
                tx,
                &context,
                &Roles,
                &ConfigScope::Workspace,
                1,
                &entries,
            )
            .map_err(|e| panic!("stage: {e:?}"))
        })
        .unwrap()
    }
}
fn proposal(result: OperationResult) -> devworks_core::protocol::records::ApprovalViewRecord {
    match result {
        OperationResult::ApprovalsInspect { value }
        | OperationResult::ApprovalsRecordHumanDecision { value }
        | OperationResult::ApprovalsPropose { value } => value,
        r => panic!("unexpected result {r:?}"),
    }
}
#[test]
fn settings_decision_requires_exact_host_human_witness_and_persists_reviewed_snapshot() {
    let f = Fx::new();
    let p = f.stage();
    let inspected = proposal(f.query("approvals.inspect", json!({"proposalId":p.id})));
    assert!(inspected.reviewed_source.is_some());
    let input = json!({"proposalId":p.id,"expectedRevision":"1","contentDigest":p.body["contentDigest"],"decision":"Approve","rationale":"Reviewed the exact change"});
    assert!(
        matches!(f.command(&f.ctx("user"),"approvals.record_human_decision",input.clone()),DomainDecision::Refused{fault}if fault.code==DomainErrorCode::PermissionDenied)
    );
    let typed = serde_json::from_value(input.clone()).unwrap();
    let bound = HostIngress::new(&f.s)
        .human_decision_context(&f.ctx("user"), &typed)
        .unwrap();
    let mut changed = input.clone();
    changed["rationale"] = json!("Changed after interaction");
    assert!(
        matches!(f.command(&bound,"approvals.record_human_decision",changed),DomainDecision::Refused{fault}if fault.code==DomainErrorCode::PermissionDenied)
    );
    let DomainDecision::Applied { result } =
        f.command(&bound, "approvals.record_human_decision", input)
    else {
        panic!("approval failed")
    };
    assert!(matches!(
        proposal(result).state,
        devworks_core::protocol::records::ApprovalState::Approved
    ));
    let audit = read_snapshot(&f.s, &ReadOptions::default(), |s| {
        s.decisions()
            .human_decision(ApprovalSource::Settings, &p.id)
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        audit.reviewed_snapshot["reviewedSource"]["entries"],
        p.body["entries"]
    );
    assert_eq!(audit.actor, actor("user"));
}
#[test]
fn stale_settings_content_cannot_approve_a_new_revision() {
    let f = Fx::new();
    let p = f.stage();
    let context = f.ctx("user");
    run_write(&f.s, WriteOptions::default(), move |tx| {
        let current = tx
            .configuration()
            .current(&ConfigScope::Workspace, "settings")?;
        assert!(current.is_none());
        tx.configuration()
            .append_revision(
                &ConfigScope::Workspace,
                "settings",
                1,
                1,
                json!({"values":{}}),
            )
            .map(|_| ())
    })
    .unwrap();
    let input = json!({"proposalId":p.id,"expectedRevision":"1","contentDigest":p.body["contentDigest"],"decision":"Approve","rationale":"Exact prior change"});
    let typed = serde_json::from_value(input.clone()).unwrap();
    let bound = HostIngress::new(&f.s)
        .human_decision_context(&context, &typed)
        .unwrap();
    assert!(matches!(
        f.command(&bound, "approvals.record_human_decision", input),
        DomainDecision::Refused { .. }
    ));
    assert!(
        read_snapshot(&f.s, &ReadOptions::default(), |s| s
            .decisions()
            .human_decision(ApprovalSource::Settings, &p.id))
        .unwrap()
        .is_none()
    );
}
#[test]
fn large_typed_plan_inspection_uses_frozen_snapshot_and_list_stays_compact_across_restart() {
    let f = Fx::new();
    let plan = json!({"content":{"artifactId":"artifact","versionId":"version","sha256":"a".repeat(64),"byteLength":"1","mediaType":"text/markdown"},"goals":vec!["x".repeat(30000);20],"requirements":[],"acceptance":[],"acceptanceTests":[],"repositoryIds":[]});
    let typed: devworks_core::protocol::records::PlanDraft =
        serde_json::from_value(plan.clone()).unwrap();
    let digest = devworks_core::services::work::plan_digest(&typed);
    let saved = plan.clone();
    run_write(&f.s, WriteOptions::default(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: devworks_core::protocol::ProjectId(id("project")),
            name: "Project".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.work().insert_work(&StoredWork {
            id: id("feature"),
            project_id: devworks_core::protocol::ProjectId(id("project")),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        })?;
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: id("feature"),
                revision: 1,
                approved: None,
                approved_by: None,
                approved_at_ms: None,
                pending: Some(json!({"draft":saved,"digest":digest})),
                pending_revision: Some(1),
            },
            None,
        )?;
        tx.work().put_proposal(
            &StoredProposal {
                id: id("plan-proposal"),
                feature_id: id("feature"),
                revision: 1,
                plan_revision: 1,
                plan_digest: digest.clone(),
                state: "Pending".into(),
                requested_by: actor("agent"),
                requested_at_ms: T0,
                decided_by: None,
                decided_at_ms: None,
                summary: "Review plan".into(),
                reviewed_snapshot: Some(json!({"draft":saved,"digest":digest})),
            },
            None,
        )?;
        Ok(())
    })
    .unwrap();
    let shown = proposal(f.query("approvals.inspect", json!({"proposalId":"plan-proposal"})));
    let value = serde_json::to_value(shown).unwrap();
    assert_eq!(value["reviewedSource"]["plan"], plan);
    assert!(value.to_string().len() > 524288);
    let listed = f.query(
        "approvals.list",
        json!({"scope":{"kind":"Workspace"},"page":{"limit":20}}),
    );
    assert!(serde_json::to_vec(&listed).unwrap().len() < 2000);
    run_write(&f.s, WriteOptions::default(), |tx| {
        let mut slot = tx.work().feature_plan(&id("feature"))?.unwrap();
        slot.revision += 1;
        slot.pending = Some(json!({"draft":{"changed":true},"digest":"changed"}));
        slot.pending_revision = Some(2);
        tx.work().put_feature_plan(&slot, Some(1))
    })
    .unwrap();
    let shown = proposal(f.query("approvals.inspect", json!({"proposalId":"plan-proposal"})));
    assert_eq!(
        serde_json::to_value(&shown).unwrap()["reviewedSource"]["plan"],
        plan
    );
    assert_eq!(shown.stale, Some(true));
    let Fx { h, s, cases, n } = f;
    drop(s);
    let reopened = h.store();
    let f = Fx {
        h,
        s: reopened,
        cases,
        n,
    };
    let shown = proposal(f.query("approvals.inspect", json!({"proposalId":"plan-proposal"})));
    assert_eq!(
        serde_json::to_value(shown).unwrap()["reviewedSource"]["plan"],
        plan
    );
}
#[test]
fn legal_multibyte_settings_snapshot_exceeds_dynamic_json_but_inspects_without_truncation() {
    let f = Fx::new();
    let entries = AgentRole::ALL
        .iter()
        .map(|role| {
            let d = descriptor(role.instructions_key()).unwrap();
            serde_json::from_value::<SettingAssignment>(
                json!({"key":d.key,"value":{"schema":d.schema_ref(),"value":"界".repeat(65536)}}),
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let wanted = serde_json::to_value(&entries).unwrap();
    assert!(wanted.to_string().len() > 1048576);
    let ctx = f.ctx("agent");
    let p = run_write(&f.s, WriteOptions::default(), move |tx| {
        devworks_core::settings::service::stage_proposal(
            tx,
            &ctx,
            &Roles,
            &ConfigScope::Workspace,
            1,
            &entries,
        )
        .map_err(|e| panic!("source refused legal instructions: {e:?}"))
    })
    .unwrap();
    let shown = proposal(f.query("approvals.inspect", json!({"proposalId":p.id})));
    assert_eq!(
        serde_json::to_value(shown).unwrap()["reviewedSource"]["entries"],
        wanted
    );
    let list = f.query(
        "approvals.list",
        json!({"scope":{"kind":"Workspace"},"page":{"limit":20}}),
    );
    assert!(serde_json::to_vec(&list).unwrap().len() < 2000);
}
