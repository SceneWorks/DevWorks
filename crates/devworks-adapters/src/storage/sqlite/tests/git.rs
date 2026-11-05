//! Real SQLite ownership, revision and historical-retention checks.
use super::Harness;
use devworks_core::ports::storage::*;
use devworks_core::protocol::data::{GitOid, ObjectFormat};
use devworks_core::protocol::{ActorId, Id, ProjectId};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
#[test]
fn owned_git_worktrees_preserve_identity_and_reject_stale_updates() {
    let h = Harness::new();
    let store = h.store();
    let owner = store.info().host_actor;
    let record = StoredGitWorktree {
        id: id("tree"),
        repository_id: id("repo"),
        work_id: id("task"),
        owner,
        ownership_revision: 1,
        owner_root_id: None,
        owner_assignment_id: None,
        registered_root_id: id("root"),
        path: "/owned/tree".into(),
        branch: "refs/heads/task-a".into(),
        head: GitOid {
            format: ObjectFormat::Sha1,
            hex: "a".repeat(40),
        },
        revision: 1,
        removed: false,
        location: None,
        pending_operation_id: None,
        last_operation_id: None,
        last_result: None,
    };
    let first = record.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id("project")),
            name: "Project".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })?;
        tx.projects().insert_repository(&StoredRepository {
            id: id("repo"),
            location: "/repo".into(),
            name: "Repo".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: 1,
        })?;
        tx.projects()
            .set_members(&ProjectId(id("project")), &[id("repo")])?;
        tx.work().insert_work(&StoredWork {
            id: id("task"),
            project_id: ProjectId(id("project")),
            kind: "task".into(),
            parent_feature_id: None,
            status_id: None,
            title: "Task".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            created_by: None,
            updated_by: None,
            search_key: "task".into(),
            caller_ref: None,
            completion: None,
            brief: serde_json::json!({}),
        })?;
        tx.work()
            .set_work_repositories(&id("task"), &[id("repo")])?;
        tx.git().insert_git_worktree(&first)
    })
    .unwrap();
    let got = read_snapshot(&store, &ReadOptions::default(), |s| {
        s.git().git_worktree(&id("tree"))
    })
    .unwrap()
    .unwrap();
    assert_eq!(got, record);
    use devworks_core::context::{HostIngress, PrincipalKind};
    use devworks_core::protocol::{Counter, Revision, RootId};
    let ingress = HostIngress::new(&store);
    let info = store.info();
    let ctx = ingress
        .context(
            info.host_actor.clone(),
            PrincipalKind::Host,
            id("invocation"),
            id("git-ingress"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("task"))),
            None,
        );
    let bound =
        devworks_core::services::git::context::bind_worktree(&store, ctx.clone(), &id("tree"))
            .unwrap();
    assert_eq!(bound.worktree().unwrap().id(), &id("tree"));
    assert_eq!(bound.worktree().unwrap().ownership_revision(), 1);
    assert!(bound.with_lineage(None, None, None).worktree().is_none());
    assert!(
        devworks_core::services::git::context::bind_worktree(
            &store,
            ctx.clone().with_lineage(None, None, None),
            &id("tree")
        )
        .is_err()
    );
    let other = ingress
        .context(
            ActorId(id("other")),
            PrincipalKind::Agent,
            id("other-invocation"),
            id("git-ingress"),
            Counter::new(info.restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_lineage(
            Some(ProjectId(id("project"))),
            Some(RootId(id("task"))),
            None,
        );
    assert!(
        devworks_core::services::git::context::bind_worktree(&store, other, &id("tree")).is_err()
    );
    run_write(&store, WriteOptions::default(), |tx| {
        tx.work().set_work_repositories(&id("task"), &[])
    })
    .unwrap();
    assert!(
        devworks_core::services::git::context::bind_worktree(&store, ctx.clone(), &id("tree"))
            .is_err()
    );
    run_write(&store, WriteOptions::default(), |tx| {
        tx.work().set_work_repositories(&id("task"), &[id("repo")])
    })
    .unwrap();
    run_write(&store, WriteOptions::default(), |tx| {
        tx.projects().set_members(&ProjectId(id("project")), &[])
    })
    .unwrap();
    assert!(
        devworks_core::services::git::context::bind_worktree(&store, ctx.clone(), &id("tree"))
            .is_err()
    );
    run_write(&store, WriteOptions::default(), |tx| {
        tx.projects()
            .set_members(&ProjectId(id("project")), &[id("repo")])
    })
    .unwrap();
    let mut changed = record.clone();
    changed.revision = 2;
    changed.head.hex = "b".repeat(40);
    let advance = changed.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.git().update_git_worktree(&advance, 1)
    })
    .unwrap();
    let stale = changed.clone();
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .git()
            .update_git_worktree(&stale, 1))
        .is_err()
    );
    for mutation in 0..3 {
        let mut wrong = changed.clone();
        wrong.revision = 3;
        match mutation {
            0 => wrong.owner = ActorId(id("other")),
            1 => wrong.path = "/primary".into(),
            _ => wrong.work_id = id("other-task"),
        };
        assert!(
            run_write(&store, WriteOptions::default(), move |tx| tx
                .git()
                .update_git_worktree(&wrong, 2))
            .is_err()
        );
    }
    changed.revision = 3;
    changed.removed = true;
    let removed = changed.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        tx.git().update_git_worktree(&removed, 2)
    })
    .unwrap();
    let retained = read_snapshot(&store, &ReadOptions::default(), |s| {
        s.git().git_worktrees(&id("task"), None, 10)
    })
    .unwrap();
    assert_eq!(retained, vec![changed.clone()]);
    assert!(
        devworks_core::services::git::context::bind_worktree(&store, ctx.clone(), &id("tree"))
            .is_err()
    );
    // History retains removed paths, but a shared repository is not permission to read a
    // different project's worktree metadata through this root's Git associations.
    let history_ctx = ctx.clone();
    let own_head = changed.head.clone();
    let foreign_template = record.clone();
    run_write(&store, WriteOptions::default(), move |tx| {
        let mut project = tx.projects().project(&ProjectId(id("project")))?.unwrap();
        project.id = ProjectId(id("project-b"));
        project.revision = 1;
        project.scope_revision = 1;
        project.name = "Other project".into();
        tx.projects().insert_project(&project)?;
        tx.projects().set_members(&project.id, &[id("repo")])?;
        let mut work = tx.work().work(&id("task"))?.unwrap();
        work.id = id("foreign-task");
        work.project_id = project.id;
        tx.work().insert_work(&work)?;
        tx.work().set_work_repositories(&work.id, &[id("repo")])?;
        let mut foreign = foreign_template.clone();
        foreign.id = id("foreign-tree");
        foreign.work_id = work.id;
        foreign.path = "/foreign/tree".into();
        foreign.branch = "refs/heads/foreign".into();
        tx.git().insert_git_worktree(&foreign)?;
        for tree in [id("tree"), id("foreign-tree")] {
            use devworks_core::services::history::{
                GitFacts, LinkKind, NewHistoryLink, record_link,
            };
            record_link(
                tx,
                &history_ctx,
                NewHistoryLink {
                    root_id: RootId(id("task")),
                    kind: LinkKind::Git,
                    git: Some(GitFacts {
                        repository_id: id("repo"),
                        worktree_id: Some(tree),
                        branch: Some("task-a".into()),
                        pull_request_url: None,
                        head: own_head.clone(),
                    }),
                    artifact: None,
                    target: None,
                    summary: "Retained Git association".into(),
                    supersedes: None,
                },
            )?
            .map_err(|r| StorageError::InvalidRecord(format!("{r:?}")))?;
        }
        Ok(())
    })
    .unwrap();
    let authority = devworks_core::authority::HostAuthority {
        store: &store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = devworks_core::gateway::Gateway {
        store: &store,
        authority: &authority,
        ids: &*h.ids,
    };
    let history = devworks_core::services::history::HistoryService::new(h.ids.clone());
    let frame = serde_json::json!({"protocol":"1.0-draft.1","schemaRevision":3,"mode":"Query","queryId":"retained-source","body":{"kind":"history.git_links","rootId":"task","page":{"limit":20}}});
    let rows = match gateway
        .query(
            &ctx,
            "history.git_links",
            frame.to_string().as_bytes(),
            &history,
        )
        .unwrap()
    {
        devworks_core::protocol::results::DomainQueryReply::Result {
            result: devworks_core::protocol::results::OperationResult::HistoryGitLinks { value },
            ..
        } => value.items.0,
        other => panic!("{other:?}"),
    };
    let own = rows
        .iter()
        .find(|l| l.worktree_id == Some(id("tree")))
        .unwrap();
    assert_eq!(own.worktree_path.as_ref().unwrap().0, "/owned/tree");
    assert_eq!(own.worktree_removed, Some(true));
    let foreign = rows
        .iter()
        .find(|l| l.worktree_id == Some(id("foreign-tree")))
        .unwrap();
    assert!(foreign.worktree_path.is_none() && foreign.worktree_removed.is_none());
    changed.revision = 4;
    changed.removed = false;
    assert!(
        run_write(&store, WriteOptions::default(), move |tx| tx
            .git()
            .update_git_worktree(&changed, 3))
        .is_err()
    );
}
