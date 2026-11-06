use super::*;
use devworks_core::ports::{
    AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization, Clock,
};

struct RealAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn Clock>,
}
impl AuthorityPort for RealAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        devworks_core::authority::HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
fn native_app() -> App {
    let mut a = App::unbooted(Harness::new(), None, None);
    let mut c = a.composition();
    c.authority = Arc::new(RealAuthority {
        store: a.store.clone(),
        clock: a.h.clock(),
    });
    c.config.approval_resume_interval = Duration::from_secs(3600);
    a.app = Some(a.rt.block_on(boot(c)).unwrap());
    a
}
fn saved(a: &App, source: &ProposalRecord) -> StoredApproval {
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Workspace,
                state: None,
            },
            None,
            500,
        )
    });
    rows.into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| {
            let row = read(&*a.store, |s| s.decisions().approval(&r.id)).unwrap();
            (row.payload.pointer("/binding/proposal/id") == Some(&json!(source.id))).then_some(row)
        })
        .expect("owner-derived saved package review")
}
fn decide(a: &App, p: &StoredApproval, decision: &str) {
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":a.key("decision"),"body":{"kind":"approvals.record_human_decision","proposalId":p.id,"expectedRevision":p.revision.to_string(),"contentDigest":p.content_digest,"decision":decision,"rationale":"Review exact signed fixture version"}});
    applied(
        a.app()
            .record_human_decision(&a.user, &serde_json::to_vec(&frame).unwrap())
            .unwrap(),
    );
}
#[test]
fn saved_package_source_rejection_uses_exact_owner_proposal() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("saved-reject", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    let review = saved(&a, &source);
    assert_eq!(review.author, *a.agent.principal());
    assert_eq!(review.author_kind, PrincipalKind::Agent);
    assert_eq!(review.state, "Pending");
    decide(&a, &review, "Reject");
    let source = read(&*a.store, |s| s.packages().proposal(&source.id)).unwrap();
    assert_eq!(source.state, "Rejected");
    let version = read(&*a.store, |s| {
        s.packages().package_version("saved-reject", &digest)
    })
    .unwrap();
    assert_eq!(version.phase, VersionPhase::Rejected);
    assert!(read(&*a.store, |s| s.decisions().approval_resume(&review.id)).is_none());
}
#[test]
fn saved_package_approval_readmits_original_agent_and_spends_exact_grant() {
    let _serial = serial();
    let a = native_app();
    let spec = Spec::new("saved-approve", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    let review = saved(&a, &source);
    decide(&a, &review, "Approve");
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    let resume = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert_eq!(resume.state, "Admitted", "{resume:?}");
    assert_eq!(resume.principal, *a.agent.principal());
    assert_eq!(resume.principal_kind, PrincipalKind::Agent);
    let version = read(&*a.store, |s| {
        s.packages().package_version("saved-approve", &digest)
    })
    .unwrap();
    assert!(version.approval.is_some(), "{version:?}");
    let source = read(&*a.store, |s| s.packages().proposal(&source.id)).unwrap();
    assert_eq!(source.state, "Approved");
    assert_eq!(source.decided_by, Some(a.user.principal().clone()));
    let grant = read(&*a.store, |s| {
        s.authority()
            .authority_grant(resume.grant_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(grant.uses_remaining, Some(0));
    a.rt.block_on(a.app().approval_resumes().tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap(),
        resume
    );
}

struct PauseBeforeReadmission {
    runtime: Arc<Runtime>,
    user: InvocationContext,
    pause: Vec<u8>,
    paused: std::sync::atomic::AtomicBool,
}
impl devworks_engine::approvals::ResumeAdmission for PauseBeforeReadmission {
    fn readmit(
        &self,
        ctx: &InvocationContext,
        operation: &str,
        raw: &[u8],
        options: WriteOptions,
    ) -> devworks_core::gateway::CommandOutcome {
        if !self.paused.swap(true, Ordering::SeqCst) {
            applied(
                self.runtime
                    .command(&self.user, "runs.pause", &self.pause)
                    .unwrap(),
            );
        }
        self.runtime.command_with(ctx, operation, raw, options)
    }
}
#[test]
fn saved_package_late_root_pause_preserves_exact_pending_source_and_unspent_grant() {
    let _serial = serial();
    let mut a = native_app();
    seed_root(&a.h, "package-root");
    rusqlite::Connection::open(a.h.root.join("workspace.sqlite"))
        .unwrap()
        .execute(
            "UPDATE work_items SET kind='issue' WHERE id='package-root'",
            [],
        )
        .unwrap();
    a.agent = a.agent.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(devworks_core::protocol::RootId(id("package-root"))),
        None,
    );
    a.rt.block_on(a.app().approval_resumes().drain());
    let spec = Spec::new("saved-paused", "1.0.0");
    let (source, digest, _) = propose(&a, &spec);
    // The source inherited the real root's project scope.
    let rows = read(&*a.store, |s| {
        s.decisions().list_approval_sources(
            &ApprovalSourceFilter {
                scope: ConfigScope::Project(ProjectId(id("project-a"))),
                state: None,
            },
            None,
            500,
        )
    });
    let review = rows
        .into_iter()
        .filter(|r| r.source == ApprovalSource::Generic)
        .find_map(|r| read(&*a.store, |s| s.decisions().approval(&r.id)))
        .unwrap();
    decide(&a, &review, "Approve");
    let runtime = Arc::new(a.app.take().unwrap());
    let admission = Arc::new(PauseBeforeReadmission {runtime:runtime.clone(),user:a.user.clone(),pause:serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":"late-package-pause","command":{"kind":"runs.pause","rootId":"package-root","expectedControlRevision":"1"}})).unwrap(),paused:std::sync::atomic::AtomicBool::new(false)});
    let dispatcher = devworks_engine::approvals::ResumeDispatcher::new(
        a.store.clone(),
        runtime.approvals().clone(),
        admission.clone(),
    );
    a.rt.block_on(dispatcher.tick()).unwrap();
    assert!(
        admission.paused.load(Ordering::SeqCst),
        "pause must happen after prepare_resume returned Ready"
    );
    let pending = read(&*a.store, |s| s.decisions().approval_resume(&review.id)).unwrap();
    assert!(
        matches!(pending.state.as_str(), "Pending" | "Unknown"),
        "{pending:?}"
    );
    let grant = read(&*a.store, |s| {
        s.authority()
            .authority_grant(pending.grant_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(grant.uses_remaining, Some(1));
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&source.id))
            .unwrap()
            .state,
        "Pending"
    );
    assert!(
        read(&*a.store, |s| s
            .packages()
            .package_version("saved-paused", &digest))
        .unwrap()
        .approval
        .is_none()
    );
    let root = devworks_core::protocol::RootId(id("package-root"));
    let control = read(&*a.store, |s| s.runs().root_control(&root)).unwrap();
    assert_eq!(control.desired, DesiredExecution::Paused);
    let holds = read(&*a.store, |s| {
        s.runs()
            .blockers(&BlockerFilter::RootLevel(root), None, 500)
    });
    let covered: Vec<_> = holds
        .iter()
        .filter(|h| h.cause.manual_resume())
        .map(|h| json!({"holdId":h.id,"revision":h.revision.to_string()}))
        .collect();
    let frame=serde_json::to_vec(&json!({"protocol":"1.0-draft.1","requestKey":"resume-package-root","command":{"kind":"runs.continue","rootId":"package-root","expectedControlRevision":control.revision.to_string(),"coveredHolds":covered}})).unwrap();
    applied(runtime.command(&a.user, "runs.continue", &frame).unwrap());
    a.rt.block_on(dispatcher.tick()).unwrap();
    assert_eq!(
        read(&*a.store, |s| s.decisions().approval_resume(&review.id))
            .unwrap()
            .state,
        "Admitted"
    );
    assert_eq!(
        read(&*a.store, |s| s.packages().proposal(&source.id))
            .unwrap()
            .state,
        "Approved"
    );
    a.rt.block_on(dispatcher.drain());
    drop(dispatcher);
    drop(admission);
    let runtime = Arc::try_unwrap(runtime)
        .unwrap_or_else(|_| panic!("runtime retained after drained dispatcher"));
    a.rt.block_on(runtime.shutdown());
}
