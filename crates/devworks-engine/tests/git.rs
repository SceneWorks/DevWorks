//! Real Git reads through the gateway, process owner, SQLite and immutable artifact service.
mod common;
use common::*;
use devworks_adapters::{
    fs::{NativeFilesystem, artifacts::NativeArtifactFiles, sources::HostArtifactSources},
    process::NativeProcessHost,
};
use devworks_core::{
    authority::HostAuthority,
    context::{HostIngress, PrincipalKind},
    gateway::Gateway,
    ports::storage::*,
    protocol::results::{DomainQueryReply, OperationResult},
    protocol::{Counter, Revision},
    services::{
        artifacts::ArtifactService,
        git::{GitConfig, GitService},
        indexing::helper::SupervisedHelpers,
        process::ProcessSupervisor,
        resources::ResourceService,
    },
};
use std::{io::Read, path::Path, process::Command, sync::Arc};
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} exited {:?}: stdout={} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
#[test]
fn native_git_inspection_and_diff_use_the_owned_process_and_artifact_boundaries() {
    let h = Harness::new();
    h.readers.store(1, std::sync::atomic::Ordering::SeqCst);
    let store = h.open();
    let source = h.root.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("a.txt"), "before\n").unwrap();
    git(&source, &["add", "a.txt"]);
    git(&source, &["commit", "-m", "before"]);
    let base = git(&source, &["rev-parse", "HEAD"]);
    std::fs::write(source.join("a.txt"), "after\n").unwrap();
    git(&source, &["commit", "-am", "after"]);
    let head = git(&source, &["rev-parse", "HEAD"]);
    std::fs::write(source.join("untracked"), "dirty").unwrap();
    let path = source.to_string_lossy().into_owned();
    write(&*store, move |tx| {
        tx.projects().insert_repository(&StoredRepository {
            id: id("repo"),
            location: path,
            name: "Repo".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        })
    })
    .unwrap();
    let filesystem = Arc::new(NativeFilesystem::new());
    let processes = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
    ));
    let helpers = Arc::new(SupervisedHelpers {
        supervisor: processes.clone(),
        store: store.clone(),
        fs: filesystem.clone(),
    });
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.join("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        filesystem,
        h.clock(),
        h.ids.clone(),
    ));
    let service = GitService::new(
        store.clone(),
        helpers,
        artifacts.clone(),
        h.ids.clone(),
        h.clock(),
        GitConfig::default(),
    );
    let info = store.info();
    let ctx = HostIngress::new(&*store).context(
        info.host_actor,
        PrincipalKind::Host,
        id("invoke"),
        id("git-test"),
        Counter::new(info.restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let authority = HostAuthority {
        store: &*store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*store,
        authority: &authority,
        ids: &*h.ids,
    };
    let query = serde_json::json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"inspect","body":{"kind":"git.inspect","repositoryId":"repo"}});
    match gateway
        .query(&ctx, "git.inspect", query.to_string().as_bytes(), &service)
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::GitInspect { value },
            ..
        } => {
            assert_eq!(value.head.hex, head);
            assert!(value.dirty);
            assert_eq!(value.branch.unwrap().0, "refs/heads/main");
        }
        other => panic!("{other:?}"),
    }
    let query = serde_json::json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"diff","body":{"kind":"git.diff","repositoryId":"repo","base":{"format":"Sha1","hex":base},"head":{"format":"Sha1","hex":head},"paths":["a.txt"],"page":{"limit":10}}});
    match gateway
        .query(&ctx, "git.diff", query.to_string().as_bytes(), &service)
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::GitDiff { value },
            ..
        } => {
            assert_eq!(value.items.0.len(), 1);
            assert!(!value.has_more);
            let mut read = artifacts
                .open_exact_version(&*store, &ctx, &value.items.0[0])
                .unwrap();
            let mut bytes = String::new();
            read.reader.read_to_string(&mut bytes).unwrap();
            assert!(bytes.contains("-before"));
            assert!(bytes.contains("+after"));
        }
        other => panic!("{other:?}"),
    }
    // Git's working-file hashing can execute a configured filter. A verified read
    // refuses it before launch, rather than running it or substituting a false dirty bit.
    std::fs::write(source.join(".gitattributes"), "a.txt filter=fixture\n").unwrap();
    git(&source, &["add", ".gitattributes"]);
    git(
        &source,
        &[
            "config",
            "filter.fixture.clean",
            "git config --local probe.executed yes",
        ],
    );
    let query = serde_json::json!({"protocol":"1.0-draft.1","mode":"Query","queryId":"filtered","body":{"kind":"git.inspect","repositoryId":"repo"}});
    let reply = gateway
        .query(&ctx, "git.inspect", query.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        serde_json::to_value(reply)
            .unwrap()
            .to_string()
            .contains("UnsupportedCapability")
    );
    let marker = Command::new("git")
        .args(["config", "--get", "probe.executed"])
        .current_dir(&source)
        .output()
        .unwrap();
    assert_eq!(
        marker.status.code(),
        Some(1),
        "verified inspection never executes the filter"
    );
    // Status may reuse clean stat data; force hashing so this positive control does not
    // depend on whether the index still considers the working file racily clean.
    git(&source, &["hash-object", "--path=a.txt", "a.txt"]);
    assert_eq!(
        git(&source, &["config", "--get", "probe.executed"]),
        "yes",
        "the fixture actually exercises executable clean filters"
    );
    // A staged gitlink change must remain dirty even when the initialized submodule agrees
    // with the index. Parent status may skip child hashing, but cannot hide staged gitlinks.
    let child = h.root.join("child");
    let parent = h.root.join("parent");
    std::fs::create_dir(&child).unwrap();
    std::fs::create_dir(&parent).unwrap();
    git(&child, &["init", "-b", "main"]);
    std::fs::write(child.join("a.txt"), "one").unwrap();
    git(&child, &["add", "."]);
    git(&child, &["commit", "-m", "one"]);
    let one = git(&child, &["rev-parse", "HEAD"]);
    std::fs::write(child.join("a.txt"), "two").unwrap();
    git(&child, &["commit", "-am", "two"]);
    git(&parent, &["init", "-b", "main"]);
    git(
        &parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child.to_str().unwrap(),
            "sub",
        ],
    );
    git(&parent, &["commit", "-am", "submodule"]);
    git(&parent.join("sub"), &["checkout", &one]);
    git(&parent, &["add", "sub"]);
    let location = parent.to_str().unwrap().to_owned();
    write(&*store, move |tx| {
        tx.projects().insert_repository(&StoredRepository {
            id: id("subrepo"),
            location: location.clone(),
            name: "Submodules".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        })
    })
    .unwrap();
    let query = serde_json::json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"staged-gitlink","body":{"kind":"git.inspect","repositoryId":"subrepo"}});
    let reply = gateway
        .query(&ctx, "git.inspect", query.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        matches!(reply,DomainQueryReply::Result { result:OperationResult::GitInspect { value },.. } if value.dirty)
    );
    std::fs::write(parent.join("sub/.gitattributes"), "a.txt filter=fixture\n").unwrap();
    git(
        &parent.join("sub"),
        &[
            "config",
            "filter.fixture.clean",
            "git config --local probe.executed yes",
        ],
    );
    std::fs::write(parent.join("sub/a.txt"), "dirty child").unwrap();
    let reply = gateway
        .query(&ctx, "git.inspect", query.to_string().as_bytes(), &service)
        .unwrap();
    assert!(
        serde_json::to_value(reply)
            .unwrap()
            .to_string()
            .contains("UnsupportedCapability")
    );
    assert_eq!(
        Command::new("git")
            .args(["config", "--get", "probe.executed"])
            .current_dir(parent.join("sub"))
            .status()
            .unwrap()
            .code(),
        Some(1)
    );
    assert!(processes.owned().is_empty(), "helper trees are reaped");
}

#[test]
fn native_worktree_creation_is_journaled_isolated_and_recoverable() {
    native_task_lifecycle(false, None);
}
#[test]
fn native_task_pr_is_observed_linked_and_recovered_after_response_loss() {
    native_task_lifecycle(true, None);
}
#[derive(Clone, Copy, Debug)]
enum ReviewedCommitCase {
    PreCommit,
    ConfiguredPreCommit,
    PostIndex,
    Signer,
    UnexpectedTree,
    UnexpectedMessage,
    ChangedHookNames,
}

#[test]
fn reviewed_commit_never_executes_traditional_or_configured_hooks_or_signer() {
    for case in [
        ReviewedCommitCase::PreCommit,
        ReviewedCommitCase::ConfiguredPreCommit,
        ReviewedCommitCase::PostIndex,
        ReviewedCommitCase::Signer,
    ] {
        native_task_lifecycle(false, Some(case));
    }
}

#[test]
fn reviewed_commit_does_not_accept_unexpected_committed_tree_or_message_on_recovery() {
    for case in [
        ReviewedCommitCase::UnexpectedTree,
        ReviewedCommitCase::UnexpectedMessage,
    ] {
        native_task_lifecycle(false, Some(case));
    }
}

#[test]
fn reviewed_commit_refuses_changed_hook_name_witness_before_dispatch() {
    native_task_lifecycle(false, Some(ReviewedCommitCase::ChangedHookNames));
}

fn native_task_lifecycle(with_pr: bool, reviewed_case: Option<ReviewedCommitCase>) {
    use devworks_core::protocol::results::DomainDecision;
    use devworks_core::protocol::{CommandReply, ProjectId, RootId};
    use devworks_core::services::git::{EffectOutcome, RegisteredGitRoot};
    let h = Harness::new();
    h.readers.store(1, std::sync::atomic::Ordering::SeqCst);
    let store = h.open();
    let source = h.root.join("source");
    let owned = h.root.join("worktrees");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir(&owned).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.invalid"]);
    git(&source, &["config", "commit.gpgsign", "false"]);
    // This fixture asserts exact LF bytes; do not inherit the host's checkout conversion.
    git(&source, &["config", "core.autocrlf", "false"]);
    std::fs::write(source.join("a.txt"), "base\n").unwrap();
    git(&source, &["add", "a.txt"]);
    git(&source, &["commit", "-m", "base"]);
    let head = git(&source, &["rev-parse", "HEAD"]);
    let remote = h.root.join("remote.git");
    let remote_url = remote.to_string_lossy().into_owned();
    git(&source, &["init", "--bare", &remote_url]);
    git(&source, &["remote", "add", "origin", &remote_url]);
    std::fs::write(source.join("a.txt"), "user change\n").unwrap();
    let path = source.to_string_lossy().into_owned();
    write(&*store, move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id("project")),
            name: "Project".into(),
            description: String::new(),
            lifecycle: ACTIVE.into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.projects().insert_repository(&StoredRepository {
            id: id("repo"),
            location: path,
            name: "Repo".into(),
            remote_url: Some(if with_pr {
                "https://github.com/o/r.git".into()
            } else {
                remote_url
            }),
            primary: true,
            lifecycle: ACTIVE.into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        })?;
        tx.projects()
            .set_members(&ProjectId(id("project")), &[id("repo")])?;
        tx.work().insert_work(&StoredWork {
            id: id("task"),
            project_id: ProjectId(id("project")),
            kind: TASK.into(),
            parent_feature_id: None,
            status_id: None,
            title: "Task".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "task".into(),
            caller_ref: None,
            completion: None,
            brief: serde_json::json!({}),
        })?;
        tx.work().set_work_repositories(&id("task"), &[id("repo")])
    })
    .unwrap();
    let filesystem = Arc::new(NativeFilesystem::new());
    let processes = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
    ));
    let helpers = Arc::new(CommitReplyLost(
        SupervisedHelpers {
            supervisor: processes.clone(),
            store: store.clone(),
            fs: filesystem.clone(),
        },
        reviewed_case,
    ));
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.join("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        filesystem.clone(),
        h.clock(),
        h.ids.clone(),
    ));
    let github = Arc::new(GitHubHttp::default());
    if with_pr {
        write(&*store, |tx| {
            let schema = devworks_core::settings::catalog::descriptor("github.credential").unwrap().schema_ref();
            tx.configuration().append_revision(&ConfigScope::Workspace, "settings", 1, 1,
                serde_json::json!({"values":{"github.credential":{"schema":schema,"value":{"secretRef":devworks_core::authority::redact::SecretRef::mint(7).as_str()}}}}))?;
            Ok(())
        }).unwrap();
    }
    let service = Arc::new(GitService::new(
        store.clone(),
        helpers,
        artifacts.clone(),
        h.ids.clone(),
        h.clock(),
        GitConfig {
            roots: vec![RegisteredGitRoot {
                id: id("owned-root"),
                path: std::fs::canonicalize(&owned).unwrap(),
            }],
            filesystem: Some(filesystem),
            github: with_pr.then(|| {
                Arc::new(devworks_core::services::git::github::GitHubClient {
                    http: github.clone(),
                    secrets: Arc::new(GitHubSecrets),
                })
            }),
            ..GitConfig::default()
        },
    ));
    let info = store.info();
    let ctx = HostIngress::new(&*store)
        .context(
            info.host_actor,
            PrincipalKind::Host,
            id("invoke"),
            id("git-test"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("task"))),
            None,
        );
    let authority = HostAuthority {
        store: &*store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: &*store,
        authority: &authority,
        ids: &*h.ids,
    };
    let frame = serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"create-tree","body":{"kind":"git.worktree","repositoryId":"repo","workId":"task","expectedRevision":"1","action":"Create","base":{"format":"Sha1","hex":head},"registeredRootId":"owned-root"}});
    if !with_pr && reviewed_case.is_none() {
        let mut cancelled = frame.clone();
        cancelled["requestKey"] = serde_json::json!("cancel-create");
        let op = match gateway
            .command(
                &ctx,
                "git.worktree",
                cancelled.to_string().as_bytes(),
                service.clone(),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Accepted { operation_id } => operation_id,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        let delivery = read(&*store, |s| {
            s.queue().deliveries(
                &DeliveryFilter {
                    consumer: None,
                    operation_id: Some(op.clone()),
                },
                None,
                10,
            )
        })
        .entries
        .remove(0);
        let pending = read(&*store, |s| s.git().git_worktrees(&id("task"), None, 10)).remove(0);
        assert!(!Path::new(&pending.path).exists());
        let cancel = serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"cancel-delivery","body":{"kind":"queue.resolve","deliveryId":delivery.id,"expectedRevision":delivery.revision.to_string(),"expectedFence":delivery.generation.to_string(),"resolution":"CancelBeforeDispatch","reason":"cancel queued creation"}});
        let reply = gateway
            .command(
                &ctx,
                "queue.resolve",
                cancel.to_string().as_bytes(),
                Arc::new(devworks_core::services::queue::QueueService),
            )
            .unwrap();
        assert!(
            matches!(reply,CommandReply::Recorded { receipt,.. } if matches!(receipt.decision,DomainDecision::Applied { .. }))
        );
        assert_eq!(
            read(&*store, |s| s.operations().operation(&op))
                .unwrap()
                .state,
            devworks_core::protocol::OperationState::CancelledBeforeDispatch
        );
        assert_eq!(service.reconcile_cancelled().unwrap(), 1);
        assert!(
            read(&*store, |s| s.git().git_worktree(&pending.id))
                .unwrap()
                .removed
        );
        assert!(
            !Path::new(&pending.path).exists(),
            "cancelled creation never runs Git"
        );
        assert_eq!(
            service.reconcile_cancelled().unwrap(),
            0,
            "cancelled reservation settles once"
        );
    }
    let operation = match gateway
        .command(
            &ctx,
            "git.worktree",
            frame.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let mut before = read(&*store, |s| s.git().git_worktrees(&id("task"), None, 10));
    before.retain(|t| !t.removed);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].pending_operation_id.as_ref(), Some(&operation));
    assert!(
        !Path::new(&before[0].path).exists(),
        "receipt precedes native dispatch"
    );
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let consumer = devworks_engine::git::consumer(service.clone());
    let dispatcher = dispatcher(&h, store.clone(), vec![consumer]);
    let claims = runtime().block_on(dispatcher.run_until_idle()).unwrap();
    assert_eq!(claims, 1);
    let settled = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    assert_eq!(
        settled.state,
        devworks_core::protocol::OperationState::ObservedComplete
    );
    let evidence = settled
        .latest_attempt()
        .unwrap()
        .observations
        .iter()
        .rev()
        .find_map(|o| o.pointer("/effect/evidence"))
        .unwrap()
        .clone();
    let outcome = EffectOutcome::Completed(evidence);
    let after = read(&*store, |s| s.git().git_worktree(&before[0].id)).unwrap();
    assert!(after.pending_operation_id.is_none());
    assert_eq!(after.head.hex, head);
    assert_eq!(
        std::fs::read_to_string(Path::new(&after.path).join("a.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        std::fs::read_to_string(source.join("a.txt")).unwrap(),
        "user change\n"
    );
    assert_eq!(git(&source, &["branch", "--show-current"]), "main");
    assert_eq!(
        service.observe(&record),
        outcome,
        "recovery reads its durable result without replay"
    );
    let tree_path = Path::new(&after.path);
    if let Some(case) = reviewed_case {
        approvals::assert_reviewed_commit(
            &h,
            store.clone(),
            (service, artifacts),
            &gateway,
            &ctx,
            &after,
            case,
        );
        return;
    }
    std::fs::write(tree_path.join("new.txt"), "new task work\n").unwrap();
    let requested_message = "\nTask implementation   \n\nBody   \n";
    let message = artifacts
        .publish_evidence(
            &*store,
            devworks_core::services::artifacts::evidence::Evidence {
                scope: ArtifactScope::Project(ProjectId(id("project"))),
                producer: ctx.principal().clone(),
                principal_kind: "Host".into(),
                agent_session: None,
                root: ctx.root_id().cloned(),
                assignment: None,
                title: "Commit message".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: serde_json::json!({"kind":"TestMessage"}),
            },
            requested_message.as_bytes(),
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let frame = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"commit-task","command":{"kind":"Commit","worktreeId":after.id,"expectedHead":after.head,"message":message}});
    let commit = match gateway
        .command(
            &ctx,
            "git.commit",
            frame.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&commit)).unwrap();
    let lost = service.execute(&record);
    assert!(matches!(lost, EffectOutcome::Unknown(_)), "{lost:?}");
    assert_eq!(
        read(&*store, |s| s.git().git_worktree(&after.id))
            .unwrap()
            .pending_operation_id
            .as_ref(),
        Some(&commit),
        "commit reply loss precedes the durable completion checkpoint"
    );
    let committed_head = git(tree_path, &["rev-parse", "HEAD"]);
    let result = service.observe(&record);
    assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
    assert_eq!(
        git(tree_path, &["rev-parse", "HEAD"]),
        committed_head,
        "recovery must not create another commit"
    );
    let raw = Command::new("git")
        .args(["cat-file", "commit", "HEAD"])
        .current_dir(tree_path)
        .output()
        .unwrap();
    assert!(raw.status.success());
    let raw = String::from_utf8(raw.stdout).unwrap();
    assert_eq!(raw.split_once("\n\n").unwrap().1, requested_message);
    let new_head = git(tree_path, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, head);
    assert_eq!(
        git(tree_path, &["show", "--format=%P", "--no-patch", "HEAD"]),
        head
    );
    assert_eq!(git(tree_path, &["show", "HEAD:new.txt"]), "new task work");
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), head);
    assert_eq!(service.observe(&record), result);
    let current = read(&*store, |s| s.git().git_worktree(&after.id)).unwrap();
    if with_pr {
        let tree_id = current.id.clone();
        write(&*store, move |tx| {
            let worker = devworks_core::protocol::ActorId(id("worker"));
            tx.actors()
                .register(&worker, PrincipalKind::Agent, "Worker")?;
            let mut task = tx.work().work(&id("task"))?.unwrap();
            let mut second = task.clone();
            second.id = id("task-2");
            second.title = "Related task".into();
            second.search_key = "related task".into();
            second.manual_order = 1;
            tx.work().insert_work(&second)?;
            tx.work().set_work_repositories(&second.id, &[id("repo")])?;
            let prior = task.revision;
            task.revision += 1;
            task.scope_revision += 1;
            task.brief = serde_json::json!({"assignedAgentId":"worker"});
            tx.work().update_work(&task, prior)?;
            let mut tree = tx.git().git_worktree(&tree_id)?.unwrap();
            let prior = tree.revision;
            tree.revision += 1;
            tree.owner = worker;
            tree.ownership_revision += 1;
            tx.git().update_git_worktree(&tree, prior)
        })
        .unwrap();
        let current = read(&*store, |s| s.git().git_worktree(&current.id)).unwrap();
        *github.sha.lock().unwrap() = new_head.clone();
        let frame = serde_json::json!({"protocol":"1.0-draft.1","schemaRevision":2,"requestKey":"task-pr","command":{"kind":"OpenPullRequest","repositoryId":"repo","headRef":current.branch,"baseRef":"refs/heads/main","expectedHead":current.head,"description":message,"coveredWorkIds":["task-2","task"]}});
        let mut legacy = frame.clone();
        legacy.as_object_mut().unwrap().remove("schemaRevision");
        assert!(
            matches!(
                gateway
                    .command(
                        &ctx,
                        "git.pull_request",
                        legacy.to_string().as_bytes(),
                        service.clone()
                    )
                    .unwrap(),
                CommandReply::Rejected { .. }
            ),
            "revision 1 must not silently drop group coverage"
        );
        let operation = match gateway
            .command(
                &ctx,
                "git.pull_request",
                frame.to_string().as_bytes(),
                service.clone(),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => match receipt.decision {
                DomainDecision::Accepted { operation_id } => operation_id,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        let mut reordered = frame.clone();
        reordered["command"]["coveredWorkIds"] = serde_json::json!(["task", "task-2"]);
        assert!(
            matches!(
                gateway
                    .command(
                        &ctx,
                        "git.pull_request",
                        reordered.to_string().as_bytes(),
                        service.clone()
                    )
                    .unwrap(),
                CommandReply::Recorded { replayed: true, .. }
            ),
            "coverage is a normalized set"
        );
        let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
        let result = service.execute(&record);
        assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
        assert_eq!(
            github.writes.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "lost POST response must not duplicate the write"
        );
        assert_eq!(service.observe(&record), result);
        let links = read(&*store, |s| {
            s.sessions()
                .links(&RootId(id("task")), &["pull_request"], None, 10)
        });
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_id, "https://github.com/o/r/pull/1");
        assert_eq!(links[0].revision.as_deref(), Some(new_head.as_str()));
        let related = read(&*store, |s| {
            s.sessions()
                .links(&RootId(id("task-2")), &["pull_request"], None, 10)
        });
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].target_id, links[0].target_id);
        let still_owned = read(&*store, |s| s.git().git_worktree(&current.id)).unwrap();
        assert_eq!(
            still_owned.owner.0,
            id("worker"),
            "PR coordination does not take the worker's code ownership"
        );
        assert_eq!(still_owned.ownership_revision, 2);
        assert!(!record.payload.to_string().contains("test-private-token"));
        assert!(processes.owned().is_empty());
        return;
    }
    let push = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"push-task","command":{"kind":"Push","worktreeId":current.id,"remoteId":"origin","targetRef":current.branch,"expectedHead":current.head,"mode":"Normal"}});
    let operation = match gateway
        .command(
            &ctx,
            "git.push",
            push.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
    assert_eq!(git(&remote, &["rev-parse", &current.branch]), new_head);
    assert_eq!(service.observe(&record), result);
    let stale = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"stale-push","command":{"kind":"Push","worktreeId":current.id,"remoteId":"origin","targetRef":current.branch,"expectedHead":current.head,"expectedRemote":{"format":"Sha1","hex":head},"mode":"Normal"}});
    let operation = match gateway
        .command(
            &ctx,
            "git.push",
            stale.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(matches!(result, EffectOutcome::Failed(_)), "{result:?}");
    assert_eq!(git(&remote, &["rev-parse", &current.branch]), new_head);
    assert!(
        read(&*store, |s| s.git().git_worktree(&current.id))
            .unwrap()
            .pending_operation_id
            .is_none(),
        "known pre-dispatch refusal releases the reservation"
    );
    assert_eq!(service.observe(&record), result);
    let bound =
        devworks_core::services::git::context::bind_worktree(&*store, ctx.clone(), &current.id)
            .unwrap();
    assert_eq!(
        bound.worktree().unwrap().ownership_revision(),
        1,
        "head observations do not revoke ownership context"
    );
    write(&*store, |tx| {
        tx.actors().register(
            &devworks_core::protocol::ActorId(id("human")),
            PrincipalKind::Human,
            "Human",
        )
    })
    .unwrap();
    let human = HostIngress::new(&*store)
        .context(
            devworks_core::protocol::ActorId(id("human")),
            PrincipalKind::Human,
            id("take-ownership"),
            id("git-test"),
            Counter::new(store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("task"))),
            None,
        );
    let frame = serde_json::json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":"attach-tree","body":{"kind":"git.worktree","repositoryId":"repo","workId":"task","expectedRevision":"1","action":"Attach","base":current.head,"registeredRootId":"owned-root","worktreeId":current.id}});
    let operation = match gateway
        .command(
            &human,
            "git.worktree",
            frame.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
    let transferred = read(&*store, |s| s.git().git_worktree(&current.id)).unwrap();
    assert_eq!(transferred.owner, *human.principal());
    assert_eq!(transferred.ownership_revision, 2);
    assert_eq!(transferred.path, current.path);
    assert_eq!(transferred.branch, current.branch);
    assert_eq!(transferred.head, current.head);
    let refreshed_reader =
        devworks_core::services::git::context::bind_worktree(&*store, ctx.clone(), &current.id)
            .unwrap();
    assert_eq!(
        refreshed_reader.worktree().unwrap().ownership_revision(),
        2,
        "host may refresh a read binding without acquiring mutation ownership"
    );
    let denied_commit = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"old-owner-commit","command":{"kind":"Commit","worktreeId":current.id,"expectedHead":current.head,"message":message}});
    assert!(
        matches!(gateway.command(&ctx, "git.commit", denied_commit.to_string().as_bytes(), service.clone()).unwrap(), CommandReply::Recorded { receipt, .. } if matches!(receipt.decision, DomainDecision::Refused { .. })),
        "old owner cannot commit after transfer"
    );
    assert_eq!(
        devworks_core::services::git::context::bind_worktree(&*store, human, &current.id)
            .unwrap()
            .worktree()
            .unwrap()
            .ownership_revision(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(source.join("a.txt")).unwrap(),
        "user change\n"
    );
    // A saved path is insufficient identity: replacing its .git pointer must not redirect
    // inspection or future writes to an unrelated checkout, even at the same commit.
    let foreign = h.root.join("foreign");
    std::fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init", "-b", "other"]);
    let marker = Path::new(&current.path).join(".git");
    let saved = std::fs::read(&marker).unwrap();
    std::fs::write(
        &marker,
        format!("gitdir: {}\n", foreign.join(".git").display()),
    )
    .unwrap();
    let query = serde_json::json!({"protocol":"1.0-draft.1","mode":"Query","queryId":"redirected","body":{"kind":"git.inspect","repositoryId":"repo","worktreeId":current.id}});
    let reply = gateway
        .query(&ctx, "git.inspect", query.to_string().as_bytes(), &*service)
        .unwrap();
    assert!(
        serde_json::to_value(reply)
            .unwrap()
            .to_string()
            .contains("SourceChanged")
    );
    std::fs::write(marker, saved).unwrap();
    // An owned agent may publish a rebased head even while the cached ledger still names
    // the previous commit. Its explicit remote lease remains the concurrency guard.
    git(
        &source,
        &["push", remote.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let tree_id = current.id.clone();
    let host = ctx.clone();
    write(&*store, move |tx| {
        let worker = devworks_core::protocol::ActorId(id("force-worker"));
        tx.actors()
            .register(&worker, PrincipalKind::Agent, "Force worker")?;
        devworks_core::authority::service::bind_principal(
            tx,
            &host,
            NewPrincipalBinding {
                actor: worker.clone(),
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: Some(ProjectId(id("project"))),
                tool_families: None,
                bound_by: host.principal().clone(),
            },
            None,
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))?;
        let mut task = tx.work().work(&id("task"))?.unwrap();
        let prior = task.revision;
        task.revision += 1;
        task.scope_revision += 1;
        task.brief = serde_json::json!({"assignedAgentId":"force-worker"});
        tx.work().update_work(&task, prior)?;
        let mut tree = tx.git().git_worktree(&tree_id)?.unwrap();
        let prior = tree.revision;
        tree.revision += 1;
        tree.owner = worker;
        tree.ownership_revision += 1;
        tree.owner_root_id = Some(RootId(id("task")));
        tree.owner_assignment_id = None;
        tx.git().update_git_worktree(&tree, prior)
    })
    .unwrap();
    let agent = HostIngress::new(&*store)
        .context(
            devworks_core::protocol::ActorId(id("force-worker")),
            PrincipalKind::Agent,
            id("force-invocation"),
            id("force-ingress"),
            Counter::new(store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("task"))),
            None,
        );
    git(tree_path, &["commit", "--amend", "-m", "Rebased task"]);
    let rebased = git(tree_path, &["rev-parse", "HEAD"]);
    assert_ne!(rebased, new_head);
    let force = serde_json::json!({"protocol":"1.0-draft.1","requestKey":"agent-rebased-force","command":{"kind":"Push","worktreeId":current.id,"remoteId":"origin","targetRef":current.branch,"expectedHead":{"format":"Sha1","hex":rebased},"expectedRemote":{"format":"Sha1","hex":new_head},"mode":"ForceWithLease"}});
    let operation = approvals::approve_owned_force(
        &h,
        store.clone(),
        service.clone(),
        &gateway,
        &agent,
        &force,
    );
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
    assert_eq!(git(&remote, &["rev-parse", &current.branch]), rebased);
    // A differently named remote default is protected too; no automatic approval can bypass it.
    git(&remote, &["symbolic-ref", "HEAD", &current.branch]);
    git(tree_path, &["commit", "--amend", "-m", "Another rebase"]);
    let another = git(tree_path, &["rev-parse", "HEAD"]);
    let mut forbidden = force;
    forbidden["requestKey"] = serde_json::json!("default-ref-force");
    forbidden["command"]["expectedHead"]["hex"] = serde_json::json!(another);
    forbidden["command"]["expectedRemote"]["hex"] = serde_json::json!(rebased);
    let operation = match gateway
        .command(
            &agent,
            "git.push",
            forbidden.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(matches!(result, EffectOutcome::Failed(_)), "{result:?}");
    assert_eq!(git(&remote, &["rev-parse", &current.branch]), rebased);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&remote, &["update-ref", "-d", &current.branch]);
    git(
        &remote,
        &["symbolic-ref", &current.branch, "refs/heads/main"],
    );
    // A host-configured legacy protocol must not hide the alias from qualification.
    git(tree_path, &["config", "protocol.version", "0"]);
    let main_before = git(&remote, &["rev-parse", "refs/heads/main"]);
    let mut alias = forbidden;
    alias["requestKey"] = serde_json::json!("symbolic-main-force");
    alias["command"]["expectedRemote"]["hex"] = serde_json::json!(main_before);
    let operation = match gateway
        .command(
            &agent,
            "git.push",
            alias.to_string().as_bytes(),
            service.clone(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let record = read(&*store, |s| s.operations().operation(&operation)).unwrap();
    let result = service.execute(&record);
    assert!(
        matches!(result, EffectOutcome::Failed(_)),
        "symbolic main alias: {result:?}"
    );
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/main"]),
        main_before,
        "an owned ref alias never grants main force authority"
    );
    assert!(processes.owned().is_empty());
}

#[derive(Default)]
struct GitHubHttp {
    sha: std::sync::Mutex<String>,
    pr: std::sync::Mutex<Option<serde_json::Value>>,
    writes: std::sync::atomic::AtomicUsize,
}
struct GitHubSecrets;
impl devworks_core::ports::secrets::SecretResolver for GitHubSecrets {
    fn resolve(
        &self,
        _: &devworks_core::authority::redact::SecretRef,
    ) -> Result<
        devworks_core::ports::secrets::SecretValue,
        devworks_core::ports::secrets::SecretUnavailable,
    > {
        Ok(devworks_core::ports::secrets::SecretValue::new(
            "test-private-token",
        ))
    }
}
impl devworks_core::ports::http::HttpTransport for GitHubHttp {
    fn send(
        &self,
        request: &devworks_core::ports::http::HttpRequest,
    ) -> Result<devworks_core::ports::http::HttpResponse, devworks_core::ports::http::HttpError>
    {
        use devworks_core::ports::http::{HttpError, HttpResponse, Method};
        use serde_json::json;
        assert!(request.url.starts_with("https://api.github.com/"));
        assert_eq!(
            request.header_value("Authorization"),
            Some("Bearer test-private-token")
        );
        assert!(!format!("{request:?}").contains("test-private-token"));
        let value = match request.method {
            Method::Get if request.url == "https://api.github.com/repos/o/r" => {
                json!({"full_name":"o/r","default_branch":"main"})
            }
            Method::Get => self
                .pr
                .lock()
                .unwrap()
                .clone()
                .map_or(json!([]), |pr| json!([pr])),
            Method::Post | Method::Patch => {
                let body: serde_json::Value =
                    serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
                let pr = json!({"number":1,"html_url":"https://github.com/o/r/pull/1","state":"open","head":{"repo":{"full_name":"o/r"},"ref":body["head"],"sha":self.sha.lock().unwrap().clone()},"base":{"repo":{"full_name":"o/r"},"ref":body["base"]},"title":body["title"],"body":body["body"]});
                *self.pr.lock().unwrap() = Some(pr);
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(HttpError::Timeout); // write happened, response lost
            }
        };
        Ok(HttpResponse {
            status: 200,
            body: serde_json::to_vec(&value).unwrap(),
        })
    }
}

/// Lose the successful commit reply before the Git owner can checkpoint it. All native
/// processes still run through the real supervisor; recovery must observe, never replay.
struct CommitReplyLost(SupervisedHelpers, Option<ReviewedCommitCase>);
impl devworks_core::services::indexing::helper::HelperRunner for CommitReplyLost {
    fn filesystem(&self) -> Option<&dyn devworks_core::authority::destructive::FilesystemResolver> {
        self.0.filesystem()
    }
    fn environment(&self) -> Vec<(String, String)> {
        self.0.environment()
    }
    fn run(
        &self,
        ctx: &devworks_core::context::InvocationContext,
        spec: devworks_core::ports::process::ProcessSpec,
        limits: devworks_core::services::indexing::helper::HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, devworks_core::services::indexing::helper::HelperFault> {
        self.0.run(ctx, spec, limits, stop)
    }
    fn run_difference(
        &self,
        ctx: &devworks_core::context::InvocationContext,
        spec: devworks_core::ports::process::ProcessSpec,
        limits: devworks_core::services::indexing::helper::HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, devworks_core::services::indexing::helper::HelperFault> {
        self.0.run_difference(ctx, spec, limits, stop)
    }
    fn run_recorded_difference(
        &self,
        ctx: &devworks_core::context::InvocationContext,
        spec: devworks_core::ports::process::ProcessSpec,
        limits: devworks_core::services::indexing::helper::HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &devworks_core::protocol::OperationId,
    ) -> Result<Vec<u8>, devworks_core::services::indexing::helper::HelperFault> {
        self.0
            .run_recorded_difference(ctx, spec, limits, stop, operation)
    }
    fn run_recorded(
        &self,
        ctx: &devworks_core::context::InvocationContext,
        spec: devworks_core::ports::process::ProcessSpec,
        limits: devworks_core::services::indexing::helper::HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &devworks_core::protocol::OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, devworks_core::services::indexing::helper::HelperFault> {
        let commit = spec.args.first().is_some_and(|arg| arg == "commit");
        let directory = spec.cwd.clone();
        let reply = self
            .0
            .run_recorded(ctx, spec, limits, stop, operation, approved_hold)?;
        if commit {
            if let Some(case) = self.1 {
                let path = directory.as_deref().unwrap();
                match case {
                    ReviewedCommitCase::UnexpectedTree => {
                        std::fs::write(path.join("unexpected.txt"), "not reviewed\n").unwrap();
                        git(path, &["add", "unexpected.txt"]);
                        git(path, &["commit", "--amend", "--no-edit"]);
                    }
                    ReviewedCommitCase::UnexpectedMessage => {
                        git(
                            path,
                            &["commit", "--amend", "-m", "not the reviewed message"],
                        );
                    }
                    _ => {}
                }
                return Ok(reply);
            }
            Err(
                devworks_core::services::indexing::helper::HelperFault::Uncertain(
                    "successful commit reply lost".into(),
                ),
            )
        } else {
            Ok(reply)
        }
    }
}

#[path = "git/approvals.rs"]
mod approvals;
