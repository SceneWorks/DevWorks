//! Actual approval/readmission and native Git: Create never authorizes repository hooks.
use super::*;
use devworks_core::{
    protocol::{ActorId, ProjectId, RootId},
    services::{
        UseCases,
        approvals::{
            ApprovalCatalog, ApprovalService, ResumePreparation, owner_preview::NativeOwnerPreviews,
        },
        runs::ControlService,
    },
};
#[derive(Clone, Copy, Debug)]
pub(super) enum CreateCase {
    Hooks,
    UnavailablePolicy,
    ChangedHookNames,
    PartialCheckout,
    ContextualPolicy,
}
#[test]
fn reviewed_worktree_create_disables_native_post_checkout_hooks() {
    native_task_lifecycle_with_create(false, None, Some(CreateCase::Hooks));
}
#[test]
fn reviewed_worktree_create_requires_qualified_native_child_ban() {
    native_task_lifecycle_with_create(
        false,
        Some(ReviewedCommitCase::UnavailablePolicy),
        Some(CreateCase::UnavailablePolicy),
    );
}
#[test]
fn reviewed_worktree_create_rechecks_captured_hook_policy_before_dispatch() {
    native_task_lifecycle_with_create(false, None, Some(CreateCase::ChangedHookNames));
}
#[test]
fn reviewed_worktree_recovery_never_completes_or_repeats_a_partial_checkout() {
    native_task_lifecycle_with_create(false, None, Some(CreateCase::PartialCheckout));
}
#[test]
fn reviewed_worktree_create_qualifies_actual_target_conditional_hook_metadata() {
    native_task_lifecycle_with_create(false, None, Some(CreateCase::ContextualPolicy));
}
pub(super) fn assert_reviewed_create(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    service: Arc<GitService>,
    gateway: &Gateway<'_>,
    host: &devworks_core::context::InvocationContext,
    target: (&Path, &serde_json::Value),
    case: CreateCase,
) {
    let (source, frame) = target;
    use devworks_core::protocol::{CommandReply, results::DomainDecision};
    let marker = h.root.join("unreviewed-post-checkout-marker");
    let shell_marker = marker
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let hook = source.join(".git/hooks/post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\nprintf ran > '{shell_marker}'\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(
        source,
        &["config", "hook.reviewed-create.event", "post-checkout"],
    );
    git(
        source,
        &[
            "config",
            "hook.reviewed-create.command",
            &format!("printf configured > '{shell_marker}'"),
        ],
    );
    // Positive control uses this exact native Git and repository hook, without confinement.
    let control = h.root.join("unconfined-control");
    git(
        source,
        &[
            "worktree",
            "add",
            "--detach",
            control.to_str().unwrap(),
            frame["body"]["base"]["hex"].as_str().unwrap(),
        ],
    );
    assert!(
        marker.exists(),
        "unconfined native post-checkout hook must execute"
    );
    git(
        source,
        &["worktree", "remove", "--force", control.to_str().unwrap()],
    );
    std::fs::remove_file(&marker).unwrap();
    if matches!(case, CreateCase::ContextualPolicy) {
        let conditional = h.root.join("conditional-hooks.config");
        std::fs::write(&conditional, format!("[hook \"target-only\"]\n event = post-checkout\n command = printf contextual > '{shell_marker}'\n")).unwrap();
        git(
            source,
            &[
                "config",
                "includeIf.gitdir:**/worktrees/**.path",
                conditional.to_str().unwrap(),
            ],
        );
        assert!(
            Command::new("git")
                .args(["config", "--get", "hook.target-only.event"])
                .current_dir(source)
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
    }
    let agent = principal(
        store.as_ref(),
        "reviewed-create-worker",
        PrincipalKind::Agent,
    )
    .with_lineage(
        Some(ProjectId(id("project"))),
        Some(RootId(id("task"))),
        None,
    );
    let binder = host.clone();
    let worker = agent.principal().clone();
    write(store.as_ref(), move |tx| {
        devworks_core::authority::service::bind_principal(
            tx,
            &binder,
            NewPrincipalBinding {
                actor: worker,
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: Some(ProjectId(id("project"))),
                tool_families: None,
                bound_by: binder.principal().clone(),
            },
            None,
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    let human = principal(store.as_ref(), "worktree-reviewer", PrincipalKind::Human);
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
    let mut input = frame["body"].clone();
    input.as_object_mut().unwrap().remove("kind");
    let request =
        devworks_core::services::approvals::requests::decode("git.worktree", 1, &input, true)
            .unwrap();
    let propose=serde_json::to_vec(&serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"review-worktree-create","body":{"kind":"approvals.propose","scope":{"kind":"Project","projectId":"project"},"operationKind":"git.worktree","target":{"id":"task","revision":"1"},"contentDigest":request.fingerprint.as_str(),"summary":"Review exact native worktree creation","payload":{"kind":"SavedOperation","schemaRevision":"1","input":input}}})).unwrap();
    let reply = cases
        .command(gateway, &agent, "approvals.propose", &propose)
        .unwrap();
    if matches!(case, CreateCase::UnavailablePolicy) {
        let wire = serde_json::to_value(reply).unwrap();
        assert_eq!(wire["receipt"]["decision"]["kind"], "Refused");
        assert_eq!(
            wire["receipt"]["decision"]["fault"]["code"],
            "UnsupportedCapability"
        );
        assert_eq!(
            wire["receipt"]["decision"]["fault"]["targetKind"],
            "git_reviewed_process_policy"
        );
        assert!(!marker.exists());
        assert!(
            read(store.as_ref(), |s| s.git().git_worktrees(
                &id("task"),
                None,
                10
            ))
            .is_empty()
        );
        return;
    }
    let proposal = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                serde_json::to_value(result).unwrap()["value"].clone()
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert!(
        !marker.exists(),
        "review preparation must not execute post-checkout hooks"
    );
    let decision = serde_json::json!({"proposalId":proposal["id"],"expectedRevision":proposal["revision"],"contentDigest":proposal["contentDigest"],"decision":"Approve","rationale":"Only this exact worktree creation, never hooks"});
    let trusted = HostIngress::new(store.as_ref())
        .human_decision_context(&human, &serde_json::from_value(decision.clone()).unwrap())
        .unwrap();
    let mut body = decision;
    body["kind"] = serde_json::json!("approvals.record_human_decision");
    let reply=cases.command(gateway,&trusted,"approvals.record_human_decision",&serde_json::to_vec(&serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,"requestKey":"approve-worktree-create","body":body})).unwrap()).unwrap();
    assert!(
        matches!(&reply,CommandReply::Recorded {receipt,..} if matches!(&receipt.decision,DomainDecision::Applied {..})),
        "{reply:?}"
    );
    let approval_id = serde_json::from_value(proposal["id"].clone()).unwrap();
    let ResumePreparation::Ready(command) = approvals
        .prepare_resume(store.as_ref(), &approval_id)
        .unwrap()
    else {
        panic!("approved Create must be ready")
    };
    assert_eq!(
        command.context.principal(),
        &ActorId(id("reviewed-create-worker"))
    );
    assert_eq!(command.context.principal_kind(), PrincipalKind::Agent);
    let reply = gateway
        .command(
            &command.context,
            "git.worktree",
            &command.frame,
            service.clone(),
        )
        .unwrap();
    let operation = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert!(
        !marker.exists(),
        "readmission does not execute native helpers"
    );
    if matches!(case, CreateCase::ChangedHookNames) {
        git(source, &["config", "hook.late.event", "post-checkout"]);
    }
    let record = read(store.as_ref(), |s| s.operations().operation(&operation)).unwrap();
    if matches!(case, CreateCase::PartialCheckout) {
        // A crash after the first two confined commands leaves a real registration and HEAD,
        // but no checkout. Reconciliation may observe it, never rerun checkout or call it done.
        let tree = read(store.as_ref(), |s| {
            s.git().git_worktrees(&id("task"), None, 10)
        })
        .pop()
        .unwrap();
        let branch = tree.branch.strip_prefix("refs/heads/").unwrap();
        git(source, &["branch", "--", branch, &tree.head.hex]);
        let checkout_argument =
            devworks_core::services::indexing::git::path_argument(Path::new(&tree.path))
                .expect("stored native worktree path is representable by Git");
        git(
            source,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "--",
                &checkout_argument,
                branch,
            ],
        );
        assert!(matches!(
            service.observe(&record),
            devworks_core::services::git::EffectOutcome::Unknown(_)
        ));
        assert!(
            !Path::new(&tree.path).join("a.txt").exists(),
            "recovery must not repeat checkout"
        );
        assert_eq!(
            read(store.as_ref(), |s| s.git().git_worktree(&tree.id))
                .unwrap()
                .pending_operation_id,
            Some(operation)
        );
        assert!(!marker.exists());
        return;
    }
    let outcome = service.execute(&record);
    assert!(
        !marker.exists(),
        "reviewed Create executed an unreviewed post-checkout hook: {outcome:?}"
    );
    if matches!(case, CreateCase::ChangedHookNames) {
        assert!(!matches!(
            outcome,
            devworks_core::services::git::EffectOutcome::Completed(_)
        ));
    } else {
        assert!(
            matches!(
                outcome,
                devworks_core::services::git::EffectOutcome::Completed(_)
            ),
            "{outcome:?}"
        );
        let trees = read(store.as_ref(), |s| {
            s.git().git_worktrees(&id("task"), None, 10)
        });
        assert_eq!(trees.len(), 1);
        if matches!(case, CreateCase::ContextualPolicy) {
            assert_eq!(
                git(
                    Path::new(&trees[0].path),
                    &["config", "--get", "hook.target-only.event"]
                ),
                "post-checkout"
            );
        }
        assert!(Path::new(&trees[0].path).join("a.txt").exists());
        assert_eq!(trees[0].owner, agent.principal().clone());
        assert_eq!(
            service.observe(&record),
            outcome,
            "receipt recovery never repeats checkout"
        );
    }
}
