use super::*;

#[test]
fn saved_commit_manifest_binds_dirty_contents_and_matches_actual_staging() {
    let h = Harness::new();
    let store = h.open();
    let dir = h.root.join("manifest");
    std::fs::create_dir(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("tracked"), "initial\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "initial"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("tracked"), "reviewed\n").unwrap();
    std::fs::write(dir.join("binary"), [0, 255, 4, 0]).unwrap();
    std::fs::write(dir.join("empty"), []).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("tracked", dir.join("link")).unwrap();
    let helpers = SupervisedHelpers {
        supervisor: Arc::new(ProcessSupervisor::new(
            store.clone(),
            Arc::new(NativeProcessHost),
            Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
        )),
        store: store.clone(),
        fs: Arc::new(NativeFilesystem::new()),
    };
    let info = store.info();
    let ctx = HostIngress::new(&*store).context(
        info.host_actor,
        PrincipalKind::Host,
        id("manifest-invoke"),
        id("manifest-session"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let capture = devworks_core::services::indexing::git::Git {
        runner: &helpers,
        ctx: &ctx,
        program: "git",
        deadline: std::time::Duration::from_secs(30),
        stop: &|| false,
    };
    let before_index = git(&dir, &["diff", "--cached", "--stat"]);
    let first = capture.commit_manifest(&dir, &head).unwrap();
    assert_eq!(capture.commit_manifest(&dir, &head).unwrap(), first);
    assert_eq!(
        git(&dir, &["diff", "--cached", "--stat"]),
        before_index,
        "preview must not stage files"
    );
    std::fs::write(dir.join("tracked"), "changed after review\n").unwrap();
    let changed = capture.commit_manifest(&dir, &head).unwrap();
    assert_ne!(
        changed, first,
        "same HEAD and dirty bit cannot identify reviewed contents"
    );
    git(&dir, &["add", "--all"]);
    assert_eq!(
        capture.staged_commit_manifest(&dir, &head).unwrap(),
        changed
    );
}

pub(super) fn approve_owned_force(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    service: Arc<GitService>,
    gateway: &Gateway<'_>,
    agent: &devworks_core::context::InvocationContext,
    force: &serde_json::Value,
) -> devworks_core::protocol::OperationId {
    use devworks_core::protocol::results::DomainDecision;
    use devworks_core::protocol::{ActorId, CommandReply};
    use devworks_core::services::{
        UseCases,
        approvals::{
            ApprovalCatalog, ApprovalService, ResumePreparation, owner_preview::NativeOwnerPreviews,
        },
        runs::ControlService,
    };
    let human = principal(&*store, "git-reviewer", PrincipalKind::Human);
    let cases = UseCases::new().with_git(service.clone());
    let approvals = Arc::new(
        ApprovalService::new(
            h.ids.clone(),
            h.clock(),
            Arc::new(devworks_core::authority::HostRoles {
                store: store.clone(),
            }),
            Arc::new(ControlService::new(h.ids.clone())),
        )
        .with_catalog(ApprovalCatalog::qualified(&cases))
        .with_owners(Arc::new(
            NativeOwnerPreviews::new(Some(service.clone()), None, None)
                .with_control(Arc::new(ControlService::new(h.ids.clone()))),
        )),
    );
    let cases = cases.with_approvals(approvals.clone());
    let input = serde_json::json!({"command":force["command"]});
    let request =
        devworks_core::services::approvals::requests::decode("git.push", 1, &input, true).unwrap();
    let tree_id: devworks_core::protocol::Id =
        serde_json::from_value(input["command"]["worktreeId"].clone()).unwrap();
    let tree = read(&*store, |s| s.git().git_worktree(&tree_id)).unwrap();
    let propose=serde_json::to_vec(&serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"saved-owned-force-proposal","schemaRevision":3,"body":{"kind":"approvals.propose","scope":{"kind":"Project","projectId":"project"},"operationKind":"git.push","target":{"id":tree.id,"revision":tree.revision.to_string()},"contentDigest":request.fingerprint.as_str(),"summary":"Exact force push","payload":{"kind":"SavedOperation","schemaRevision":"1","input":input}}})).unwrap();
    let reply = cases
        .command(gateway, agent, "approvals.propose", &propose)
        .unwrap();
    let p = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let decision = serde_json::json!({"proposalId":p["id"],"expectedRevision":p["revision"],"contentDigest":p["contentDigest"],"decision":"Approve","rationale":"Review this exact owned force lease"});
    let typed = serde_json::from_value(decision.clone()).unwrap();
    let trusted = HostIngress::new(&*store)
        .human_decision_context(&human, &typed)
        .unwrap();
    let mut body = decision;
    body["kind"] = serde_json::json!("approvals.record_human_decision");
    let decision_frame=serde_json::to_vec(&serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"approve-owned-force","body":body})).unwrap();
    let reply = cases
        .command(
            gateway,
            &trusted,
            "approvals.record_human_decision",
            &decision_frame,
        )
        .unwrap();
    assert!(
        matches!(
            reply,
            CommandReply::Recorded {
                receipt: devworks_core::protocol::Receipt {
                    decision: DomainDecision::Applied { .. },
                    ..
                },
                ..
            }
        ),
        "{reply:?}"
    );
    let approval_id = serde_json::from_value(p["id"].clone()).unwrap();
    let command = match approvals.prepare_resume(&*store, &approval_id).unwrap() {
        ResumePreparation::Ready(c) => c,
        _ => panic!("approved force must be ready"),
    };
    assert_eq!(command.context.principal(), agent.principal());
    assert_eq!(command.context.principal_kind(), PrincipalKind::Agent);
    let reply = gateway
        .command(
            &command.context,
            "git.push",
            &command.frame,
            service.clone(),
        )
        .unwrap();
    let operation = match &reply {
        CommandReply::Recorded { receipt, .. } => match &receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id.clone(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    approvals
        .settle_resume(&*store, &command, &Ok(reply))
        .unwrap();
    let row = read(&*store, |s| s.decisions().approval_resume(&approval_id)).unwrap();
    let grant = read(&*store, |s| {
        s.authority()
            .authority_grant(row.grant_id.as_ref().unwrap())
    })
    .unwrap();
    assert_eq!(grant.kind, GrantKind::NativeExecution);
    assert_eq!(grant.uses_remaining, Some(0));
    let replay = gateway
        .command(&command.context, "git.push", &command.frame, service)
        .unwrap();
    assert!(
        matches!(replay, CommandReply::Recorded { replayed: true, .. }),
        "{replay:?}"
    );
    assert_eq!(
        read(&*store, |s| s.authority().authority_grant(&grant.id)).unwrap(),
        grant
    );
    assert_eq!(row.principal, ActorId(id("force-worker")));
    operation
}
