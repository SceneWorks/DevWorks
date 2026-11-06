use super::*;

#[test]
fn saved_commit_manifest_honors_repository_filemode_for_new_files() {
    for honor_mode in [false, true] {
        let h = Harness::new();
        let store = h.open();
        let dir = h.root.join("filemode-manifest");
        std::fs::create_dir(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        git(
            &dir,
            &[
                "config",
                "core.fileMode",
                if honor_mode { "true" } else { "false" },
            ],
        );
        std::fs::write(dir.join("tracked"), "initial\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);
        let head = git(&dir, &["rev-parse", "HEAD"]);
        // Header-like file content must remain byte-exact when only the mode is normalized.
        std::fs::write(dir.join("executable"), "new file mode 100755\n#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("binary"), [0, 255, 4, 0]).unwrap();
        std::fs::write(dir.join("empty"), []).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in ["executable", "binary", "empty"] {
                std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            std::os::unix::fs::symlink("tracked", dir.join("link")).unwrap();
        }
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
            id("filemode-invoke"),
            id("filemode-session"),
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
        let reviewed = capture.commit_manifest(&dir, &head).unwrap();
        assert_eq!(
            git(&dir, &["diff", "--cached", "--stat"]),
            "",
            "review cannot stage files"
        );
        git(&dir, &["add", "--all"]);
        assert_eq!(
            capture.staged_commit_manifest(&dir, &head).unwrap(),
            reviewed,
            "unchanged reviewed files must match actual staging with core.fileMode={honor_mode}"
        );
        git(&dir, &["commit", "-m", "reviewed mode semantics"]);
        let expected = if honor_mode && cfg!(unix) {
            "100755"
        } else {
            "100644"
        };
        let tree = git(&dir, &["ls-tree", "HEAD", "executable", "binary", "empty"]);
        assert!(
            tree.lines().all(|line| line.starts_with(expected)),
            "{tree}"
        );
        assert_eq!(
            git(&dir, &["show", "HEAD:executable"]),
            "new file mode 100755\n#!/bin/sh"
        );
        #[cfg(unix)]
        assert!(git(&dir, &["ls-tree", "HEAD", "link"]).starts_with("120000"));
    }
}

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
    let outsider = principal(&*store, "unowned-force-worker", PrincipalKind::Agent);
    let binder = human.clone();
    write(&*store, move |tx| {
        devworks_core::authority::service::bind_principal(
            tx,
            &binder,
            NewPrincipalBinding {
                actor: ActorId(id("unowned-force-worker")),
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: Some(devworks_core::protocol::ProjectId(id("project"))),
                tool_families: None,
                bound_by: binder.principal().clone(),
            },
            None,
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    let outsider =
        outsider.with_lineage(agent.project_id().cloned(), agent.root_id().cloned(), None);
    let mut unowned = force.clone();
    unowned["requestKey"] = serde_json::json!("unowned-force-no-grant");
    let denied = gateway
        .command(
            &outsider,
            "git.push",
            &serde_json::to_vec(&unowned).unwrap(),
            service.clone(),
        )
        .unwrap();
    assert!(
        matches!(
            denied,
            CommandReply::Rejected {
                code: devworks_core::protocol::Rejection::PermissionDenied,
                ..
            }
        ),
        "unowned Full force must require separate operation consent: {denied:?}"
    );
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

fn assert_filtered_untracked_manifest_is_nonexecuting(driver: &str, file: &std::ffi::OsStr) {
    use devworks_core::services::indexing::git::{Git, GitFault};
    let h = Harness::new();
    let store = h.open();
    let dir = h.root.join("filtered-manifest");
    std::fs::create_dir(&dir).unwrap();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitattributes"), "*.probe filter=probe\n").unwrap();
    git(&dir, &["add", ".gitattributes"]);
    git(&dir, &["commit", "-m", "attributes"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let path = dir.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "untracked filtered contents\n").unwrap();
    let command = if driver == "clean" {
        "printf ran > .git/filter-marker; cat"
    } else {
        "printf ran > .git/filter-marker; exit 1"
    };
    git(
        &dir,
        &["config", &format!("filter.probe.{driver}"), command],
    );
    // Positive control is the exact no-index diff used by manifest capture. Neither
    // --no-ext-diff nor --no-textconv disables clean/process conversion.
    let output = Command::new("git")
        .args([
            "diff",
            "--no-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--no-renames",
            "--",
            "/dev/null",
        ])
        .arg(file)
        .current_dir(&dir)
        .output()
        .unwrap();
    let marker = dir.join(".git/filter-marker");
    assert!(
        marker.exists(),
        "fixture did not execute {driver}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_file(&marker).unwrap();
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
        id("filtered-manifest-invoke"),
        id("filtered-manifest-session"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let capture = Git {
        runner: &helpers,
        ctx: &ctx,
        program: "git",
        deadline: std::time::Duration::from_secs(30),
        stop: &|| false,
    };
    let result = capture.commit_manifest(&dir, &head);
    assert!(
        !marker.exists(),
        "approval preparation executed the {driver} filter for {file:?}: {result:?}"
    );
    assert!(
        matches!(result, Err(GitFault::Unqualified)),
        "filtered untracked path must be refused before diff: {result:?}"
    );
    assert_eq!(
        git(&dir, &["diff", "--cached", "--stat"]),
        "",
        "qualification never stages contents"
    );
}
#[test]
fn saved_commit_manifest_rejects_untracked_clean_and_process_filters_before_execution() {
    for driver in ["clean", "process"] {
        assert_filtered_untracked_manifest_is_nonexecuting(
            driver,
            std::ffi::OsStr::new("new.probe"),
        );
    }
}
#[cfg(unix)]
#[test]
fn saved_commit_manifest_qualifies_nul_separated_untracked_filter_paths() {
    for file in [
        std::ffi::OsString::from("sub dir/line\n name.probe"),
        std::ffi::OsString::from("tab\tname.probe"),
    ] {
        assert_filtered_untracked_manifest_is_nonexecuting("clean", &file);
    }
}
