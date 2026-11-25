//! A real Git conflict and remote ref, with one recovery of the previously admitted publication.
use super::*;
use crate::code_review_tests::git;
use devworks_core::protocol::{EventId, provider::ArtifactRef};
use devworks_core::services::artifacts::evidence::Evidence;
use devworks_core::services::git::{
    GitConfig, GitService, RegisteredGitRoot, helper::SupervisedHelpers,
};
use devworks_core::services::process::tools::{
    RegisteredExecutable, RegisteredWorkingRoot, ToolBindings,
};
use std::path::{Path, PathBuf};

fn staged(reply: DomainCommandReply) -> OperationId {
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Accepted { operation_id } => operation_id,
            other => panic!("native Git admission: {other:?}"),
        },
        other => panic!("native Git receipt: {other:?}"),
    }
}
fn refused(reply: DomainCommandReply, target: &str) {
    assert!(
        matches!(&reply, CommandReply::Recorded {receipt,..}
        if matches!(&receipt.decision,DomainDecision::Refused {fault}
            if fault.target_kind.as_ref().is_some_and(|kind| kind.0==target))),
        "{reply:?}"
    );
}
async fn settled(fx: &Fx, id: &OperationId) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let current = read(&*fx.store, |s| s.operations().operation(id)).unwrap();
            match current.state {
                OperationState::ObservedComplete => return,
                OperationState::Failed | OperationState::NeedsInput => panic!("{current:?}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected settlement: {:?}",
            read(&*fx.store, |s| s.operations().operation(id))
        )
    });
}
fn artifact(
    fx: &Fx,
    artifacts: &devworks_core::services::artifacts::ArtifactService,
    contents: &[u8],
) -> ArtifactRef {
    artifacts
        .publish_evidence(
            &*fx.store,
            Evidence {
                scope: ArtifactScope::Project(ProjectId(id("project-a"))),
                producer: fx.user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: Some(rid("root-f")),
                assignment: None,
                title: "Feature integration fixture".into(),
                media_type: "application/json".into(),
                tags: vec![],
                source: json!({"kind":"Fixture"}),
            },
            contents,
            |_, _| Ok(()),
        )
        .unwrap()
        .0
}
fn head(path: &Path) -> String {
    git(path, &["rev-parse", "HEAD"])
}

#[test]
fn feature_task_conflict_verification_and_push_recovery_keep_the_review_and_complete_once() {
    feature_task_publication(Recovery::Accepted);
}

#[test]
fn feature_task_recovery_holds_the_dependent_when_another_repository_registration_changed() {
    feature_task_publication(Recovery::ReboundRepository);
}

#[test]
fn feature_task_recovery_holds_the_dependent_without_authoritative_publication() {
    feature_task_publication(Recovery::Unobserved);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Recovery {
    Accepted,
    ReboundRepository,
    Unobserved,
}

fn feature_task_publication(recovery: Recovery) {
    let _case = native_runtime_case();
    let fx = Fx::new();
    runtime().block_on(async {
        let project = ProjectId(id("project-a"));
        let repository = id("feature-delivery-repo");
        let fixture_root = std::fs::canonicalize(&fx.h.root).unwrap();
        let source = fixture_root.join("source");
        let remote = fixture_root.join("remote.git");
        let worktrees = fixture_root.join("owned");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&worktrees).unwrap();
        git(&source,&["init","-qb","main"]);
        std::fs::create_dir(source.join("checks")).unwrap();
        std::fs::write(source.join("checks/behavior"),"base\n").unwrap();
        git(&source,&["add","checks"]);
        git(&source,&["commit","-qm","Base"]);
        let base = head(&source);
        git(&source,&["init","-q","--bare","-b","main",remote.to_str().unwrap()]);
        git(&source,&["remote","add","origin",remote.to_str().unwrap()]);
        git(&source,&["push","-qu","origin","main"]);
        fx.tx({
            let (repository,project,source,remote)=(repository.clone(),project.clone(),source.clone(),remote.clone());
            move |_,tx| {
                tx.projects().insert_repository(&StoredRepository {
                    id:repository.clone(),location:source.to_string_lossy().into_owned(),name:"Feature fixture".into(),
                    remote_url:Some(remote.to_string_lossy().into_owned()),primary:true,lifecycle:ACTIVE.into(),
                    languages:vec![],revision:1,registered_at_ms:T0,
                })?;
                tx.projects().set_members(&project,std::slice::from_ref(&repository))?;
                for work in ["root-f","task-f1","task-f2"] {
                    tx.work().set_work_repositories(&id(work),std::slice::from_ref(&repository))?;
                }
                tx.work().set_prerequisites(&id("task-f2"),&[id("task-f1")])?;
                Ok(())
            }
        });
        let fs = Arc::new(NativeFilesystem::new());
        let git_config = || GitConfig {
            transport:Some(Arc::new(devworks_adapters::git_transport::NativeGitTransportFiles)),
            roots:vec![RegisteredGitRoot {id:id("owned-root"),path:worktrees.clone()}],
            filesystem:Some(fs.clone()), ..Default::default()
        };
        let build=|fx:&Fx| {
            let mut composed=composition_with(fx,None,Arc::new(FakePower::default()),
                Arc::new(FeatureAuthority{store:fx.store.clone(),clock:fx.h.clock()}),
                Arc::new(devworks_adapters::process::NativeProcessHost));
            composed.git=Some(devworks_engine::git::GitComposition {config:git_config(),github_http:None});
            composed.application.history = true;
            composed.config.process_tools=Some(devworks_engine::runtime::ProcessToolComposition {
                bindings:ToolBindings::new([RegisteredExecutable{id:id("check"),path:"/bin/sh".into()}],
                    Vec::<RegisteredWorkingRoot>::new()).unwrap(),
                filesystem:fs.clone(),protected_roots:ProtectedRoots::new(vec![],vec![],None),
                secrets:Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            });
            composed
        };
        let app=boot(build(&fx)).await.unwrap();
        let owner=fx.user.clone().with_lineage(Some(project.clone()),Some(rid("root-f")),None);
        let oid=|hex:&str| json!({"format":"Sha1","hex":hex});
        let tree=|work:&str| read(&*fx.store,|s| Ok(s.git().git_worktrees(&id(work),None,3)?.remove(0)));
        let worktree=|key:&str,work:&str,action:&str,commit:&str,existing:Option<&Id>| {
            let mut body=json!({"kind":"git.worktree","repositoryId":repository,"workId":work,
                "expectedRevision":"1","action":action,"base":oid(commit),"registeredRootId":"owned-root"});
            if let Some(existing)=existing { body["worktreeId"]=json!(existing); }
            let scoped = fx.user.clone().with_lineage(Some(project.clone()),Some(rid(work)),None);
            app.command(&scoped,"git.worktree",json!({"mode":"Command","protocol":PROTOCOL,
                "requestKey":key,"body":body}).to_string().as_bytes()).unwrap()
        };
        let push=|key:&str, feature:&StoredGitWorktree, predecessor:Option<&str>, candidate:&str, proof:Option<&Value>| {
            let mut command=json!({"kind":"Push","mode":"Normal","worktreeId":feature.id,"remoteId":"origin",
                "targetRef":feature.branch,"expectedHead":oid(candidate)});
            if let Some(predecessor)=predecessor { command["expectedRemote"]=oid(predecessor); }
            if let Some(proof)=proof { command["taskIntegration"]=proof.clone(); }
            json!({"protocol":PROTOCOL,"schemaRevision":16,"requestKey":key,"command":command})
        };
        let submit=|frame:&Value| app.command(&owner,"git.push",frame.to_string().as_bytes()).unwrap();

        settled(&fx,&staged(worktree("feature-checkout","root-f","Create",&base,None))).await;
        let feature=tree("root-f");
        assert!(feature.branch.starts_with("refs/heads/feature/"));
        refused(worktree("task-before-branch-push","task-f1","Create",&base,None),"published_feature_branch");
        settled(&fx,&staged(submit(&push("publish-feature-branch",&feature,None,&base,None)))).await;
        settled(&fx,&staged(worktree("task-checkout","task-f1","Create",&base,None))).await;
        let task=tree("task-f1");
        let feature_path=PathBuf::from(&feature.path);
        let task_path=PathBuf::from(&task.path);

        std::fs::write(task_path.join("checks/behavior"),"task behavior\n").unwrap();
        git(&task_path,&["commit","-qam","Task behavior"]);
        let task_head=head(&task_path);
        settled(&fx,&staged(worktree("observe-task-head","task-f1","Inspect",&base,Some(&task.id)))).await;
        let review_target=json!({"taskId":"task-f1","diffs":[{"repositoryId":repository,
            "worktreeId":task.id,"base":oid(&base),"head":oid(&task_head)}]});
        let review_report=artifact(&fx,app.artifacts(),b"{\"findings\":[]}");
        fx.tx({
            let (project,review_target,actor)=(project.clone(),review_target.clone(),owner.principal().clone());
            move |_,tx| {
                let digest="a".repeat(64);
                let review=id("fixture-independent-review");
                tx.git().put_code_review(&StoredCodeReview {
                    id:review.clone(),project_id:project,root_id:rid("root-f"),revision:1,state:"Ready".into(),
                    body:json!({"value":{"id":review,"rootId":"root-f","revision":"1","state":"Ready",
                        "findingIds":[],"headDigest":digest,"sources":[],"localReview":{
                            "target":review_target,"diffDigest":digest,"state":"Completed","findingCount":"0",
                            "reviewerActorId":"fixture-reviewer","reviewerSessionId":"fixture-review-session",
                            "completedAt":"1700000000000","report":review_report
                        }},"target":{"kind":"Local","target":review_target},"requested_by":actor,
                        "revision_of_policy":1,"source_artifacts":[],"local_binding":null,"operation":"fixture-independent-review-op",
                        "restore_epoch":0,"failure":null,"final_rounds":[],"verified_candidates":[]}),
                },None)
            }
        });

        std::fs::write(feature_path.join("checks/behavior"),"existing behavior\n").unwrap();
        git(&feature_path,&["commit","-qam","Existing feature behavior"]);
        let predecessor=head(&feature_path);
        git(&feature_path,&["push","-q","origin",&format!("HEAD:{}",feature.branch)]);
        settled(&fx,&staged(submit(&push("observe-predecessor",&tree("root-f"),Some(&predecessor),&predecessor,None)))).await;
        let run=|key:&str,tree:&StoredGitWorktree,directory:&str,script:&str| staged(app.command(&owner,"process.execute",json!({
            "mode":"Command","protocol":PROTOCOL,"requestKey":key,"body":{"kind":"process.execute",
                "spec":{"executableRef":"check","args":["-c",script],
                "workingDirectory":{"rootId":tree.id,"relative":directory},"environment":[],"profile":"Full",
                "timeoutMs":"10000","outputLimitBytes":"4096"}}
        }).to_string().as_bytes()).unwrap());
        let merge=run("merge-reviewed-task",&feature,"checks",&format!(
            "git -c user.name=Fixture -c user.email=fixture@example.test merge --no-ff --no-commit {task_head}"
        ));
        settled(&fx,&merge).await;
        let conflicted=std::fs::read_to_string(feature_path.join("checks/behavior")).unwrap();
        assert!(conflicted.contains("<<<<<<<"),"{conflicted}");
        let resolution=run("resolve-feature-conflict",&feature,"checks",
            "printf '%s\\n' 'existing behavior' 'task behavior' > behavior && git add behavior && git -c user.name=Fixture -c user.email=fixture@example.test -c commit.gpgsign=false commit -qm 'Preserve both behaviors'");
        settled(&fx,&resolution).await;
        let candidate=head(&feature_path);
        settled(&fx,&staged(worktree("observe-candidate","root-f","Inspect",&predecessor,Some(&feature.id)))).await;
        let feature=tree("root-f");
        refused(submit(&push("unverified-publication",&feature,Some(&predecessor),&candidate,None)),"feature_integration");
        assert_eq!(git(&remote,&["rev-parse",&feature.branch]),predecessor);
        assert!(read(&*fx.store,|s|s.work().work(&id("task-f1"))).unwrap().completion.is_none());

        assert_eq!(read(&*fx.store,|s|s.work().open_prerequisites(&id("task-f2"))),vec![id("task-f1")]);
        let check=|key:&str,script:&str| run(key,&feature,"checks",script);
        let checked=check("check-both-behaviors","grep -Fx 'existing behavior' behavior && grep -Fx 'task behavior' behavior");
        let failed=check("failing-focused-check","grep -Fx 'absent behavior' behavior");
        settled(&fx,&failed).await;
        settled(&fx,&checked).await;
        let observed=read(&*fx.store,|s|s.operations().operation(&checked)).unwrap();
        assert!(observed.latest_attempt().unwrap().observations.iter().any(|note| {
            note["effect"]["evidence"]["verifiedWorktree"]["head"]==oid(&candidate)
                && note["effect"]["evidence"]["result"]["value"]["exitCode"]==0
        }),"{observed:?}");
        let verification=|check:&OperationId| artifact(&fx,app.artifacts(),json!({"taskId":"task-f1","reviewId":"fixture-independent-review",
            "repositoryId":repository,"predecessor":oid(&predecessor),"taskHead":oid(&task_head),"candidate":oid(&candidate),
            "preservesTaskBehavior":true,"preservesFeatureBehavior":true,"focusedChecks":[check]}).to_string().as_bytes());
        let proof=|check:&OperationId| json!({"taskId":"task-f1","expectedTaskRevision":"1","taskWorktreeId":task.id,
            "reviewId":"fixture-independent-review","reviewedBase":oid(&base),"reviewedHead":oid(&task_head),
            "taskHead":oid(&task_head),"predecessor":oid(&predecessor),"verification":verification(check),
            "focusedChecks":[check]});
        refused(submit(&push("failed-publication",&feature,Some(&predecessor),&candidate,Some(&proof(&failed)))),
            "feature_integration");
        assert_eq!(git(&remote,&["rev-parse",&feature.branch]),predecessor);
        let publication=push("publish-reviewed-task",&feature,Some(&predecessor),&candidate,Some(&proof(&checked)));
        let artifacts=app.artifacts().clone();
        app.shutdown().await;
        drop(app);
        let operation={
            let processes=Arc::new(devworks_core::services::process::ProcessSupervisor::new(
                fx.store.clone(),Arc::new(devworks_adapters::process::NativeProcessHost),
                Arc::new(devworks_core::services::resources::ResourceService::new(fx.h.ids.clone(),fx.h.clock()))
            ));
            let service=Arc::new(GitService::new(fx.store.clone(),
                Arc::new(SupervisedHelpers{supervisor:processes,store:fx.store.clone(),fs:fs.clone()}),
                artifacts,fx.h.ids.clone(),fx.h.clock(),git_config()));
            let authority=fx.authority();
            let gateway=Gateway{authority:&authority,store:fx.store.as_ref(),ids:fx.h.ids.as_ref()};
            staged(gateway.command(&owner,"git.push",publication.to_string().as_bytes(),service).unwrap())
        };
        fx.tx({
            let operation=operation.clone();
            move |_,tx| {
                let delivery = tx.queue().claim_ready(&ClaimRequest {
                    consumer: devworks_core::services::git::EFFECT_CONSUMER.into(),
                    owner: id("interrupted-dispatcher"), lease_ms: 30_000, limit: 1,
                })?.remove(0);
                assert_eq!(delivery.operation_id.as_ref(),Some(&operation));
                let record=tx.operations().operation(&operation)?.unwrap();
                let attempt=tx.operations().begin_attempt(&operation,record.revision,AttemptBinding::default())?;
                let record=tx.operations().operation(&operation)?.unwrap();
                tx.operations().transition(&operation,record.revision,Some(&attempt.fence()),OperationState::Dispatching,None)?;
                Ok(())
            }
        });
        if recovery == Recovery::ReboundRepository {
            let other=fx.h.root.join("other-repository");
            std::fs::create_dir(&other).unwrap();
            fx.tx({
                let (project,repository,task_head)=(project.clone(),repository.clone(),task_head.clone());
                let actor=owner.principal().clone();
                move |_,tx| {
                    let other_id=id("other-delivery-repo");
                    tx.projects().insert_repository(&StoredRepository {
                        id: other_id.clone(), location:other.to_string_lossy().into_owned(),
                        name:"Other repository".into(),remote_url:Some("https://github.com/fixture/other.git".into()),
                        primary:false,lifecycle:ACTIVE.into(),languages:vec![],revision:1,registered_at_ms:T0,
                    })?;
                    tx.projects().set_members(&project,&[repository.clone(),other_id.clone()])?;
                    tx.work().set_work_repositories(&id("task-f1"),&[repository,other_id.clone()])?;
                    tx.history().append(NewEvent {
                        id:EventId(id("previous-repository-publication")),actor,project_id:Some(project),
                        root_id:Some(rid("root-f")),operation_id:None,aggregate_kind:"feature_task_publication".into(),
                        aggregate_id:devworks_core::fingerprint::sha256_hex(b"task-f1/other-delivery-repo"),
                        aggregate_revision:1,kind:"FeatureTaskPublished".into(),
                        payload:json!({"featureId":"root-f","repositoryId":other_id,"repositoryRevision":1,
                            "reviewId":"fixture-independent-review","taskRevision":"1","taskHead":{"format":"Sha1","hex":task_head}}),
                    })?;
                    assert_eq!(tx.projects().touch_repository(&other_id,1)?,2);
                    Ok(())
                }
            });
        }
        // The server accepts the already-journaled candidate before the process can store it.
        if recovery != Recovery::Unobserved {
            git(&feature_path,&["push","-q","origin",&format!("HEAD:{}",feature.branch)]);
        }
        assert!(read(&*fx.store,|s|s.work().work(&id("task-f1"))).unwrap().completion.is_none());
        let (h,old)=fx.into_parts(); drop(old);
        let store=h.open();
        let fx=Fx::over(h,store);
        let app=boot(build(&fx)).await.unwrap();
        if recovery == Recovery::Unobserved {
            app.dispatcher().observe_unknown().await.unwrap();
            let held = read(&*fx.store, |s|s.operations().operation(&operation)).unwrap();
            assert_eq!(held.state, OperationState::OutcomeUnknown);
            assert_eq!(held.attempts.len(), 1);
            assert_eq!(git(&remote,&["rev-parse",&feature.branch]),predecessor);
            assert!(read(&*fx.store,|s|s.work().work(&id("task-f1"))).unwrap().completion.is_none());
            assert_eq!(read(&*fx.store,|s|s.work().open_prerequisites(&id("task-f2"))),vec![id("task-f1")]);
            app.shutdown().await;
            return;
        }
        settled(&fx,&operation).await;
        assert_eq!(git(&remote,&["rev-parse",&feature.branch]),candidate);
        assert_eq!(read(&*fx.store,|s|s.operations().operation(&operation)).unwrap().attempts.len(),1);
        if recovery == Recovery::ReboundRepository {
            assert!(read(&*fx.store,|s|s.work().work(&id("task-f1"))).unwrap().completion.is_none());
            assert_eq!(read(&*fx.store,|s|s.work().open_prerequisites(&id("task-f2"))),vec![id("task-f1")]);
            app.shutdown().await;
            return;
        }
        assert_eq!(read(&*fx.store,|s|s.work().work(&id("task-f1"))).unwrap().completion.as_deref(),Some(VERIFIED));
        assert!(read(&*fx.store,|s|s.work().open_prerequisites(&id("task-f2"))).is_empty());
        let pending = read(&*fx.store, |s|s.operations().operation(&operation)).unwrap();
        let delivery: Id = serde_json::from_value(pending.payload["delivery_id"].clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while read(&*fx.store, |s|s.queue().entry(&delivery)).unwrap().state != QueueState::Completed {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("reconciled delivery releases its partition");
        let history=read(&*fx.store,|s|Ok(s.history().page(&EventFilter::Aggregate {
            kind:"feature_task_publication".into(),
            id:devworks_core::fingerprint::sha256_hex(b"task-f1/feature-delivery-repo"),
        },0,10)?.events));
        let recorded:Vec<_>=history.iter().filter(|event|event.kind=="FeatureTaskPublished").collect();
        assert_eq!(recorded.len(),1);
        assert_eq!(recorded[0].payload["candidate"],oid(&candidate));
        assert_eq!(recorded[0].payload["reviewedHead"],oid(&task_head));
        assert_eq!(recorded[0].payload["predecessor"],oid(&predecessor));
        assert_eq!(read(&*fx.store,|s|s.git().code_review(&id("fixture-independent-review"))).unwrap().revision,1);
        let scoped = fx.user.clone().with_lineage(Some(project.clone()),Some(rid("task-f1")),None);
        let links = result(&app.query(&scoped,"history.git_links",json!({"mode":"Query","protocol":PROTOCOL,
            "schemaRevision":3,"queryId":"integration-links","body":{"kind":"history.git_links","rootId":"task-f1","page":{"limit":100}}}
        ).to_string().as_bytes()).unwrap());
        assert!(links["items"].as_array().unwrap().iter().any(|link| {
            link["worktreeId"] == json!(feature.id) && link["head"] == oid(&candidate) && link["branch"] == feature.branch
        }),"{links}");
        let evidence = result(&app.query(&scoped,"history.evidence_links",json!({"mode":"Query","protocol":PROTOCOL,
            "queryId":"integration-evidence","body":{"kind":"history.evidence_links","rootId":"task-f1","page":{"limit":100}}}
        ).to_string().as_bytes()).unwrap());
        let evidence=evidence["items"].as_array().unwrap();
        assert!(evidence.iter().any(|item| item["artifact"]==recorded[0].payload["resolutionDiff"]));
        assert!(evidence.iter().any(|item| item["artifact"]==recorded[0].payload["verification"]));
        let scoped = fx.user.clone().with_lineage(Some(project),Some(rid("task-f2")),None);
        let next=staged(app.command(&scoped,"git.worktree",json!({"mode":"Command","protocol":PROTOCOL,
            "requestKey":"next-eligible-task","body":{"kind":"git.worktree","repositoryId":repository,"workId":"task-f2",
                "expectedRevision":"1","action":"Create","base":oid(&candidate),"registeredRootId":"owned-root"}}
        ).to_string().as_bytes()).unwrap());
        settled(&fx,&next).await;
        app.shutdown().await;
    });
}

struct FeatureAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn devworks_core::ports::Clock>,
}
impl devworks_core::ports::AuthorityPort for FeatureAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &devworks_core::ports::AuthorityRequest<'_>,
    ) -> Result<devworks_core::ports::Authorization, devworks_core::ports::AuthorityDenial> {
        HostAuthority {
            store: self.store.as_ref(),
            clock: self.clock.as_ref(),
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}
