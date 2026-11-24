use super::*;
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::protocol::data::{GitOid, ObjectFormat};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{
    CommandReply, DomainErrorCode, EventId, OperationId, OperationState, ProjectId, RootId,
};
use devworks_core::services::artifacts::evidence::Evidence;
use devworks_core::services::effect::EffectOutcome;
use devworks_core::services::git::{RegisteredGitRoot, github::GitHubClient};
use serde_json::json;

#[test]
fn final_feature_pr_requires_integrated_acceptance_and_recovers_one_remote_creation() {
    let h = Harness::new();
    let store = h.open();
    let source = h.root.join("source");
    let roots = h.root.join("owned");
    let location = roots.join("feature");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&roots).unwrap();
    git(&source, &["init", "-qb", "main"]);
    std::fs::write(source.join("file"), "accepted\n").unwrap();
    git(&source, &["add", "file"]);
    git(&source, &["commit", "-qm", "Approved feature"]);
    git(
        &source,
        &[
            "worktree",
            "add",
            "-qb",
            "feature/fixture",
            location.to_str().unwrap(),
        ],
    );
    let head = git(&location, &["rev-parse", "HEAD"]);
    let oid = GitOid {
        format: ObjectFormat::Sha1,
        hex: head.clone(),
    };
    let project = ProjectId(id("project"));
    let root = RootId(id("feature"));
    let user = principal(store.as_ref(), "feature-owner", PrincipalKind::Human).with_lineage(
        Some(project.clone()),
        Some(root.clone()),
        None,
    );
    let fs = Arc::new(NativeFilesystem::new());
    let identity = |path: &str| {
        fs.resolve(path, None)
            .unwrap()
            .target
            .unwrap()
            .identity
            .unwrap()
    };
    let path = std::fs::canonicalize(&location)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let actual_remote = "https://github.com/o/r.git";
    let tree = StoredGitWorktree {
        id: id("feature-tree"),
        repository_id: id("repo"),
        work_id: root.0.clone(),
        owner: user.principal().clone(),
        ownership_revision: 1,
        owner_root_id: Some(root.clone()),
        owner_assignment_id: None,
        registered_root_id: id("owned-root"),
        path: path.clone(),
        branch: "refs/heads/feature/fixture".into(),
        head: oid.clone(),
        revision: 1,
        removed: false,
        location: Some(GitWorktreeLocation {
            root: identity(&path),
            git_dir: identity(&git(&location, &["rev-parse", "--absolute-git-dir"])),
            common_dir: identity(&git(
                &location,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )),
        }),
        pending_operation_id: None,
        last_operation_id: None,
        last_result: None,
    };
    write(store.as_ref(),{
        let (oa,root,project,source,tree)=(user.principal().clone(),root.clone(),project.clone(),source.clone(),tree.clone());
        move |tx| {
            tx.projects().insert_project(&StoredProject {
                id:project.clone(),name:"Feature fixture".into(),description:String::new(),lifecycle:ACTIVE.into(),
                revision:1,scope_revision:1,created_at_ms:T0,updated_at_ms:T0,archived_at_ms:None,
                deleted_at_ms:None,archive_epoch:0,archive_stop_pending:false,
            })?;
            tx.projects().insert_repository(&StoredRepository {id:id("repo"),location:source.to_string_lossy().into_owned(),
                name:"Repo".into(),remote_url:Some(actual_remote.into()),primary:true,lifecycle:ACTIVE.into(),
                languages:vec![],revision:1,registered_at_ms:T0})?;
            tx.projects().set_members(&project,&[id("repo")])?;
            for (name,kind,parent,completion) in [
                ("feature",FEATURE,None,None),
                ("task",TASK,Some(root.0.clone()),Some(VERIFIED.to_owned())),
            ] {
                tx.work().insert_work(&StoredWork {id:id(name),project_id:project.clone(),kind:kind.into(),
                    parent_feature_id:parent,status_id:None,title:name.into(),priority:0,manual_order:0,
                    revision:1,scope_revision:1,created_at_ms:T0,updated_at_ms:T0,created_by:None,updated_by:None,
                    search_key:name.into(),caller_ref:None,completion,brief:json!({}),
                })?;
                tx.work().set_work_repositories(&id(name),&[id("repo")])?;
            }
            let key="github.credential";
            let schema=devworks_core::settings::catalog::descriptor(key).unwrap().schema_ref();
            tx.configuration().append_revision(&ConfigScope::Workspace,"settings",1,1,
                json!({"values":{"github.credential":{"schema":schema,"value":{"secretRef":devworks_core::authority::redact::SecretRef::mint(7).as_str()}}}}))?;
            tx.git().insert_git_worktree(&tree)?;
            let operation=OperationId(id("published-feature-head"));
            tx.operations().record_intent(NewOperation {
                id:operation.clone(),root_id:Some(root.clone()),assignment_id:None,capability:"git.push".into(),
                schema_version:1,args_digest:"fixture".into(),payload:json!({"repository_revision":1,
                    "action":{"Push":{"url":actual_remote,"target":tree.branch,"expected":tree.head}}}),
            })?;
            let attempt=tx.operations().begin_attempt(&operation,1,AttemptBinding::default())?;
            let prior=tx.operations().operation(&operation)?.unwrap();
            let dispatched=tx.operations().transition(&operation,prior.revision,Some(&attempt.fence()),OperationState::Dispatching,None)?;
            tx.operations().transition(&operation,dispatched.revision,Some(&attempt.fence()),OperationState::ObservedComplete,
                Some(json!({"effect":{"outcome":"completed","evidence":{"result":{"kind":"git.push","value":{"head":tree.head}}}}})))?;
            for (event,kind,aggregate,data) in [
                ("feature-observed","GitWriteObserved","git_worktree",json!({"repositoryId":"repo","branch":tree.branch,"head":tree.head})),
                ("task-integrated","FeatureTaskPublished","feature_task_publication",json!({"featureId":root.0,"repositoryId":"repo","branch":tree.branch,"candidate":tree.head,"repositoryRevision":1})),
            ] {
                let aggregate_id=if aggregate=="git_worktree" {tree.id.as_str().to_owned()}
                    else {devworks_core::fingerprint::sha256_hex(b"task/repo")};
                tx.history().append(NewEvent{id:EventId(id(event)),actor:oa.clone(),project_id:Some(project.clone()),
                    root_id:Some(root.clone()),operation_id:Some(operation.clone()),aggregate_kind:aggregate.into(),
                    aggregate_id,aggregate_revision:1,kind:kind.into(),payload:data})?;
            }
            Ok(())
        }
    }).unwrap();
    let wire = Arc::new(GitHubHttp::default());
    *wire.sha.lock().unwrap() = head.clone();
    let client = Arc::new(GitHubClient {
        http: wire.clone(),
        secrets: Arc::new(GitHubSecrets),
    });
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.join("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        fs.clone(),
        h.clock(),
        h.ids.clone(),
    ));
    let make = |store: &Arc<dyn ApplicationStore>| {
        Arc::new(GitService::new(
            store.clone(),
            Arc::new(SupervisedHelpers {
                supervisor: Arc::new(ProcessSupervisor::new(
                    store.clone(),
                    Arc::new(NativeProcessHost),
                    Arc::new(ResourceService::new(h.ids.clone(), h.clock())),
                )),
                store: store.clone(),
                fs: fs.clone(),
            }),
            artifacts.clone(),
            h.ids.clone(),
            h.clock(),
            GitConfig {
                github: Some(client.clone()),
                filesystem: Some(fs.clone()),
                roots: vec![RegisteredGitRoot {
                    id: id("owned-root"),
                    path: std::fs::canonicalize(&roots).unwrap(),
                }],
                ..Default::default()
            },
        ))
    };
    let description = artifacts
        .publish_evidence(
            store.as_ref(),
            Evidence {
                scope: ArtifactScope::Project(project.clone()),
                producer: user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: Some(root.clone()),
                assignment: None,
                title: "Feature description".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind":"Fixture"}),
            },
            b"Carry task",
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let admit = |store: &Arc<dyn ApplicationStore>, key: &str, covered: Option<&[&str]>| {
        let authority = HostAuthority {
            store: store.as_ref(),
            clock: h.clock.as_ref(),
            protected_branches: &[],
        };
        let gateway = Gateway {
            store: store.as_ref(),
            authority: &authority,
            ids: h.ids.as_ref(),
        };
        let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":16,"requestKey":key,"command":{
            "kind":"OpenPullRequest","repositoryId":"repo","headRef":tree.branch,"baseRef":"refs/heads/main",
            "expectedHead":oid,"description":description}});
        if let Some(covered) = covered {
            frame["command"]["coveredWorkIds"] = json!(covered);
        }
        match gateway
            .command(
                &user,
                "git.pull_request",
                frame.to_string().as_bytes(),
                make(store),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => receipt.decision,
            other => panic!("{other:?}"),
        }
    };
    assert!(
        matches!(admit(&store,"before-feature-acceptance",None),DomainDecision::Refused{fault}
        if fault.code==DomainErrorCode::ApprovalRequired)
    );
    assert_eq!(wire.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
    let (plan_content, approver) = (description.clone(), user.principal().clone());
    write(store.as_ref(), move |tx| {
        let mut feature = tx.work().work(&id("feature"))?.unwrap();
        feature.revision = 2;
        feature.brief = json!({"acceptance":[{"id":"feature-case","text":"Integrated behavior",
            "state":"Met","evidenceIds":["previous-verification"]}],
            "acceptanceGitHeads":{"feature-case":"previous-head-digest"}});
        let event = EventId(id("verification-event"));
        let sequence = tx.history().append(NewEvent {
            id: event.clone(),
            actor: approver.clone(),
            project_id: Some(feature.project_id.clone()),
            root_id: Some(RootId(feature.id.clone())),
            operation_id: None,
            aggregate_kind: "verification".into(),
            aggregate_id: "verification".into(),
            aggregate_revision: 1,
            kind: "VerificationCaptured".into(),
            payload: json!({}),
        })?;
        tx.sessions().insert_link(&StoredLink {
            id: id("integrated-verification"),
            project_id: feature.project_id.clone(),
            root_id: RootId(feature.id.clone()),
            kind: "evidence".into(),
            target_kind: "artifact_version".into(),
            target_id: plan_content.version_id.as_str().into(),
            parent_id: None,
            supersedes_id: None,
            actor: approver.clone(),
            event_id: event.0,
            sequence,
            repository_id: Some(id("repo")),
            revision: None,
            summary: "Verified integrated commit".into(),
            body: json!({"artifact":plan_content}),
            created_at_ms: T0,
        })?;
        tx.work().put_feature_plan(
            &StoredFeaturePlan {
                feature_id: feature.id.clone(),
                revision: 1,
                approved: Some(json!({"draft":{"content":plan_content,"acceptance":[{
                "id":"feature-case","text":"Integrated behavior","state":"Unmet","evidenceIds":[]
            }],"repositoryIds":["repo"],"goals":[],"requirements":[],"acceptanceTests":[]},
                "digest":"fixture-plan","planRevision":1,"proposalId":"approved-feature-plan"})),
                approved_by: Some(approver),
                approved_at_ms: Some(T0),
                pending: None,
                pending_revision: None,
            },
            None,
        )?;
        tx.work().update_work(&feature, 1)
    })
    .unwrap();
    assert!(
        matches!(admit(&store, "stale-feature-acceptance", None), DomainDecision::Refused { fault }
        if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "integrated_feature_acceptance"))
    );
    let authority = HostAuthority {
        store: store.as_ref(),
        clock: h.clock.as_ref(),
        protected_branches: &[],
    };
    let gateway = Gateway {
        store: store.as_ref(),
        authority: &authority,
        ids: h.ids.as_ref(),
    };
    let work = Arc::new(devworks_core::services::work::WorkService::new(
        h.ids.clone(),
        Arc::new(devworks_core::services::runs::ControlService::new(
            h.ids.clone(),
        )),
    ));
    let results = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":3,
        "requestKey":"record-final-feature-acceptance","body":{"kind":"features.edit","workId":"feature",
        "expectedRevision":"2","changes":{"acceptanceEvidence":[{"id":"feature-case",
        "text":"Integrated behavior","state":"Met","evidenceIds":["integrated-verification"]}]}}});
    let reply = gateway
        .command(&user, "features.edit", results.to_string().as_bytes(), work)
        .unwrap();
    assert!(
        matches!(&reply,CommandReply::Recorded{receipt,..} if matches!(receipt.decision,DomainDecision::Applied{..})),
        "{reply:?}"
    );
    assert!(
        matches!(admit(&store,"incomplete-feature-coverage",Some(&["omitted-task"])),DomainDecision::Refused{fault}
        if fault.target_kind.as_ref().is_some_and(|kind| kind.0 == "feature_pr_scope"))
    );
    let operation = match admit(&store, "open-qualified-feature", None) {
        DomainDecision::Accepted { operation_id } => operation_id,
        rejected => panic!("{rejected:?}"),
    };
    let record = write(store.as_ref(), {
        let operation = operation.clone();
        move |tx| {
            let record = tx.operations().operation(&operation)?.unwrap();
            let attempt = tx.operations().begin_attempt(
                &operation,
                record.revision,
                AttemptBinding::default(),
            )?;
            let record = tx.operations().operation(&operation)?.unwrap();
            tx.operations().transition(
                &operation,
                record.revision,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                None,
            )
        }
    })
    .unwrap();
    wire.hide_created_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let outcome = make(&store).execute(&record);
    assert!(matches!(outcome, EffectOutcome::Unknown(_)), "{outcome:?}");
    assert_eq!(wire.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(store);
    let recovered = h.open();
    let record = read(recovered.as_ref(), |s| s.operations().operation(&operation)).unwrap();
    let service = make(&recovered);
    assert!(matches!(
        service.observe(&record),
        EffectOutcome::Completed(_)
    ));
    assert!(matches!(
        service.observe(&record),
        EffectOutcome::Completed(_)
    ));
    assert_eq!(wire.writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    let event = read(recovered.as_ref(), |s| {
        Ok(s.history()
            .page(
                &EventFilter::Aggregate {
                    kind: "git_worktree".into(),
                    id: tree.id.as_str().into(),
                },
                0,
                15,
            )?
            .events
            .into_iter()
            .filter(|event| event.kind == "GitPullRequestObserved")
            .collect::<Vec<_>>())
    });
    assert_eq!(event.len(), 1);
    assert_eq!(
        event[0].payload["pullRequestUrl"],
        "https://github.com/o/r/pull/1"
    );
    for work in ["feature", "task"] {
        let links = read(recovered.as_ref(), |s| {
            s.sessions()
                .links(&RootId(id(work)), &["pull_request"], None, 10)
        });
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].body["head"], json!(oid));
        assert_eq!(links[0].target_id, "https://github.com/o/r/pull/1");
    }
}
