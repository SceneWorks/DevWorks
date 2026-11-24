//! Native final merge with real isolated checkouts and a scripted asynchronous GitHub remote.
use super::*;
use devworks_core::authority::destructive::FilesystemResolver;
use devworks_core::ports::http::{HttpError, HttpRequest, HttpResponse, HttpTransport, Method};
use devworks_core::protocol::{
    CommandReply, EventId, OperationId, OperationState, ProjectId, RootId, results::DomainDecision,
};
use devworks_core::services::{
    artifacts::evidence::Evidence,
    effect::EffectOutcome,
    git::{RegisteredGitRoot, github::GitHubClient},
    github::GitHubService,
};
use serde_json::{Value, json};
use std::sync::Mutex;

struct RemoteRepo {
    name: &'static str,
    base: String,
    head: String,
    merge: String,
    green: bool,
    merged: bool,
    accepted: bool,
}
struct Remote {
    repos: Mutex<Vec<RemoteRepo>>,
    submissions: Mutex<Vec<String>>,
    requests: Mutex<Vec<String>>,
}

impl HttpTransport for Remote {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.requests.lock().unwrap().push(request.url.clone());
        let endpoint = request.url.strip_prefix("https://api.github.com/").unwrap();
        let path = endpoint.split('?').next().unwrap();
        let segments: Vec<_> = path.split('/').collect();
        let mut repos = self.repos.lock().unwrap();
        let repo = repos
            .iter_mut()
            .find(|repo| Some(&repo.name) == segments.get(2))
            .unwrap();
        let scope = format!("o/{}", repo.name);
        assert_eq!(
            request.header_value("Authorization"),
            Some("Bearer test-private-token")
        );
        let value = match (request.method, &segments[3..]) {
            (Method::Put, ["pulls", "1", "merge-async"]) => {
                assert_eq!(
                    request.header_value("X-GitHub-Api-Version"),
                    Some("2026-03-10")
                );
                let args: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
                assert_eq!(
                    args,
                    json!({"sha":repo.head,"merge_action":"direct_merge","merge_method":"merge","bypass_rules":false})
                );
                assert!(!repo.accepted, "a merge is never repeated");
                repo.accepted = true;
                self.submissions.lock().unwrap().push(repo.name.into());
                if repo.name == "first" {
                    return Err(HttpError::Timeout);
                }
                repo.merged = true;
                json!({"status":"merged","details":{"sha":repo.merge}})
            }
            (Method::Get, []) => json!({"full_name":scope,"permissions":{"push":true},
                "allow_merge_commit":true,"archived":false,"disabled":false}),
            (Method::Get, ["pulls", "1"]) => {
                json!({"number":1,"html_url":format!("https://github.com/{scope}/pull/1"),
                "state":if repo.merged {"closed"} else {"open"},"draft":false,"mergeable":true,"mergeable_state":"clean",
                "merged":repo.merged,"merge_commit_sha":repo.merge,"base":{"repo":{"full_name":scope},"ref":"main","sha":repo.base},
                "head":{"repo":{"full_name":scope},"ref":"feature/owned","sha":repo.head}})
            }
            (Method::Get, ["git", "ref", "heads", "main"]) => {
                json!({"ref":"refs/heads/main","object":{
                "type":"commit","sha":if repo.merged {&repo.merge} else {&repo.base}}})
            }
            (Method::Get, ["git", "commits", sha]) if *sha == repo.merge => {
                json!({"sha":repo.merge,"parents":[{"sha":repo.base},{"sha":repo.head}]})
            }
            (Method::Get, ["branches", "main", "protection"]) => {
                json!({"required_status_checks":{"checks":[{"context":"native","app_id":7}]}})
            }
            (Method::Get, ["rules", "branches", "main"]) => json!([]),
            (Method::Get, ["commits", sha, "check-runs"]) if *sha == repo.head => {
                json!({"check_runs":[{
                "head_sha":repo.head,"name":"native","app":{"id":7},"status":"completed",
                "conclusion":if repo.green {"success"} else {"failure"}}]})
            }
            (Method::Get, ["commits", sha, "status"]) if *sha == repo.head => {
                json!({"sha":repo.head,"statuses":[]})
            }
            (_, path) => panic!("unexpected GitHub delivery endpoint {path:?}"),
        };
        Ok(HttpResponse {
            status: 200,
            body: value.to_string().into_bytes(),
        })
    }
}

fn begin(store: &dyn ApplicationStore, op: &OperationId) -> OperationRecord {
    write(store, {
        let op = op.clone();
        move |tx| {
            let row = tx.operations().operation(&op)?.unwrap();
            let attempt =
                tx.operations()
                    .begin_attempt(&op, row.revision, AttemptBinding::default())?;
            let row = tx.operations().operation(&op)?.unwrap();
            tx.operations().transition(
                &op,
                row.revision,
                Some(&attempt.fence()),
                OperationState::Dispatching,
                None,
            )
        }
    })
    .unwrap()
}

fn mark_complete(store: &dyn ApplicationStore, id: &OperationId, effect: EffectOutcome) {
    let EffectOutcome::Completed(value) = effect else {
        panic!("{effect:?}");
    };
    write(store, {
        let id = id.clone();
        move |tx| {
            let row = tx.operations().operation(&id)?.unwrap();
            let fence = row.latest_attempt().map(|attempt| attempt.fence());
            tx.operations().transition(
                &id,
                row.revision,
                fence.as_ref(),
                OperationState::ObservedComplete,
                Some(json!({"effect":{"outcome":"completed","evidence":value}})),
            )?;
            Ok(())
        }
    })
    .unwrap();
}

#[test]
fn native_final_merge_holds_all_repositories_and_recovers_partial_delivery_without_repeating() {
    let h = Harness::new();
    let store = h.open();
    let user = principal(store.as_ref(), "delivery-owner", PrincipalKind::Human);
    let project = ProjectId(id("delivery-project"));
    let root = RootId(id("final-feature"));
    let user = user.with_lineage(Some(project.clone()), Some(root.clone()), None);
    let fs = Arc::new(NativeFilesystem::new());
    let owned = h.root.join("owned");
    std::fs::create_dir(&owned).unwrap();
    let actor = user.principal().clone();
    let identify = |path: &str| {
        fs.resolve(path, None)
            .unwrap()
            .target
            .unwrap()
            .identity
            .unwrap()
    };
    let mut checkouts = Vec::new();
    for name in ["first", "second"] {
        let source = h.root.join(name);
        let location = owned.join(name);
        std::fs::create_dir(&source).unwrap();
        git(&source, &["init", "-qb", "main"]);
        std::fs::write(source.join("file"), "old\n").unwrap();
        git(&source, &["add", "file"]);
        git(&source, &["commit", "-qm", "Base revision"]);
        let base = git(&source, &["rev-parse", "HEAD"]);
        git(
            &source,
            &[
                "worktree",
                "add",
                "-qb",
                "feature/owned",
                location.to_str().unwrap(),
            ],
        );
        std::fs::write(location.join("file"), "reviewed\n").unwrap();
        git(&location, &["commit", "-qam", "Reviewed feature"]);
        let head = git(&location, &["rev-parse", "HEAD"]);
        let contents = git(&source, &["rev-parse", &format!("{head}^{{tree}}")]);
        let merge = git(
            &source,
            &[
                "commit-tree",
                &contents,
                "-p",
                &base,
                "-p",
                &head,
                "-m",
                "Final integration",
            ],
        );
        let path = std::fs::canonicalize(&location)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let tree = StoredGitWorktree {
            id: id(&format!("tree-{name}")),
            repository_id: id(name),
            work_id: root.0.clone(),
            owner: actor.clone(),
            ownership_revision: 1,
            owner_root_id: Some(root.clone()),
            owner_assignment_id: None,
            registered_root_id: id("owned-root"),
            path: path.clone(),
            branch: "refs/heads/feature/owned".into(),
            head: serde_json::from_value(json!({"format":"Sha1","hex":head})).unwrap(),
            revision: 1,
            removed: false,
            location: Some(GitWorktreeLocation {
                root: identify(&path),
                git_dir: identify(&git(&location, &["rev-parse", "--absolute-git-dir"])),
                common_dir: identify(&git(
                    &location,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )),
            }),
            pending_operation_id: None,
            last_operation_id: None,
            last_result: None,
        };
        checkouts.push((name, source, tree, base, merge));
    }
    let repos = checkouts
        .iter()
        .map(|(name, _, tree, base, merge)| RemoteRepo {
            name,
            base: base.clone(),
            head: tree.head.hex.clone(),
            merge: merge.clone(),
            green: *name == "first",
            merged: false,
            accepted: false,
        })
        .collect();
    let wire = Arc::new(Remote {
        repos: Mutex::new(repos),
        submissions: Mutex::new(Vec::new()),
        requests: Mutex::new(Vec::new()),
    });
    let client = Arc::new(GitHubClient {
        http: wire.clone(),
        secrets: Arc::new(GitHubSecrets),
    });
    write(store.as_ref(),{
        let (project,root,actor,checkouts)=(project.clone(),root.clone(),actor.clone(),checkouts.clone());
        move |tx| {
            tx.projects().insert_project(&StoredProject {id:project.clone(),name:"Final delivery".into(),description:String::new(),
                lifecycle:ACTIVE.into(),revision:1,scope_revision:1,created_at_ms:T0,updated_at_ms:T0,
                archived_at_ms:None,deleted_at_ms:None,archive_epoch:0,archive_stop_pending:false})?;
            let credential=devworks_core::settings::catalog::descriptor("github.credential").unwrap().schema_ref();
            let review=devworks_core::settings::catalog::descriptor("delivery.finalPullRequestReview").unwrap().schema_ref();
            let credential_revision=tx.configuration().append_revision(&ConfigScope::Workspace,"settings",1,1,json!({"values":{
                "github.credential":{"schema":credential,"value":{"secretRef":devworks_core::authority::redact::SecretRef::mint(7).as_str()}},
                "delivery.finalPullRequestReview":{"schema":review,"value":false}}}))?.revision;
            tx.github().put_github_account(&StoredGithubAccount {id:id("github-account"),revision:1,state:"Connected".into(),
                remote_user_id:Some("7".into()),login:Some("delivery-owner".into()),credential_revision,permission_revision:1,
                granted_scopes:vec!["repo".into()],granted_scopes_reported:true,observed_at_ms:T0,last_error:None},None)?;
            tx.work().insert_work(&StoredWork {id:root.0.clone(),project_id:project.clone(),kind:FEATURE.into(),parent_feature_id:None,
                status_id:None,title:"Final feature".into(),priority:0,manual_order:0,revision:1,scope_revision:1,created_at_ms:T0,
                updated_at_ms:T0,created_by:Some(actor.clone()),updated_by:Some(actor.clone()),search_key:"feature".into(),caller_ref:None,
                completion:None,brief:json!({})})?;
            for (index,(name,location,tree,base,_)) in checkouts.iter().enumerate() {
                let remote=format!("https://github.com/o/{name}.git");
                let url=format!("https://github.com/o/{name}");
                let canonical=id(&format!("github-{name}"));
                let numeric=10+index;
                tx.projects().insert_repository(&StoredRepository {id:id(name),location:location.to_string_lossy().into_owned(),
                    name:name.to_string(),remote_url:Some(remote.clone()),primary:true,lifecycle:ACTIVE.into(),languages:vec![],revision:1,registered_at_ms:T0})?;
                tx.github().put_github_repository(&StoredGithubRepository {id:canonical.clone(),account_id:id("github-account"),revision:1,
                    remote_repository_id:numeric.to_string(),owner:"o".into(),name:name.to_string(),owner_kind:"Organization".into(),
                    url:url.clone(),default_branch:Some("main".into()),permissions:json!({"pull":true,"push":true}),permission_revision:1,
                    observed_at_ms:T0},None)?;
                let identity=tx.github().append_github_observation(&StoredGithubObservation{id:id(&format!("identity-{name}")),repository_id:canonical.clone(),
                    kind:"Repository".into(),subject_id:numeric.to_string(),remote_revision:"1".into(),url:url.clone(),head:None,
                    payload:json!({"requestedRemote":remote,"repository":{"id":numeric,"owner":{"login":"o"},"name":name}}),source_updated_at_ms:None,observed_at_ms:T0})?;
                tx.github().put_github_repository_binding(&StoredGithubRepositoryBinding{repository_id:id(name),github_repository_id:canonical.clone(),
                    revision:1,repository_revision:1,registered_remote:remote,observation_id:identity.id,state:"Active".into()},None)?;
                tx.github().append_github_observation(&StoredGithubObservation{id:id(&format!("pull-{name}")),repository_id:canonical,
                    kind:"PullRequest".into(),subject_id:"1".into(),remote_revision:"1".into(),url:format!("{url}/pull/1"),
                    head:Some(tree.head.hex.clone()),payload:json!({"state":"open","baseRef":"main","baseHead":base}),
                    source_updated_at_ms:Some(T0),observed_at_ms:T0})?;
                tx.git().insert_git_worktree(tree)?;
                let event=EventId(id(&format!("pr-event-{name}")));
                let sequence=tx.history().append(NewEvent{id:event.clone(),actor:actor.clone(),project_id:Some(project.clone()),
                    root_id:Some(root.clone()),operation_id:None,aggregate_kind:"git_worktree".into(),aggregate_id:tree.id.as_str().into(),
                    aggregate_revision:1,kind:"GitPullRequestObserved".into(),payload:json!({})})?;
                tx.sessions().insert_link(&StoredLink{id:id(&format!("pr-link-{name}")),project_id:project.clone(),root_id:root.clone(),
                    kind:"pull_request".into(),target_kind:"pull_request".into(),target_id:format!("{url}/pull/1"),parent_id:None,
                    supersedes_id:None,actor:actor.clone(),event_id:event.0,sequence,repository_id:Some(id(name)),revision:Some(tree.head.hex.clone()),
                    summary:"Fixture final pull request".into(),body:json!({"worktreeId":tree.id,"branch":tree.branch,"head":tree.head}),created_at_ms:T0})?;
            }
            let members=[id("first"),id("second")];
            tx.projects().set_members(&project,&members)?;
            tx.work().set_work_repositories(&root.0,&members)?;
            Ok(())
        }
    }).unwrap();
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
    let evidence = artifacts
        .publish_evidence(
            store.as_ref(),
            Evidence {
                scope: ArtifactScope::Project(project.clone()),
                producer: actor.clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: Some(root.clone()),
                assignment: None,
                title: "Integrated acceptance".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind":"Fixture"}),
            },
            b"Reviewed and verified",
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let plan_digest = "a".repeat(64);
    write(store.as_ref(),{let (evidence,root,actor,plan_digest)=(evidence.clone(),root.clone(),actor.clone(),plan_digest.clone());move |tx| {
        tx.work().put_feature_plan(&StoredFeaturePlan {feature_id:root.0.clone(),revision:1,
            approved:Some(json!({"draft":{"content":evidence,"acceptance":[],"repositoryIds":["first","second"],"goals":[],"requirements":[],"acceptanceTests":[]},
                "digest":plan_digest,"planRevision":1,"proposalId":"approved-feature-plan"})),approved_by:Some(actor),approved_at_ms:Some(T0),
            pending:None,pending_revision:None},None)
    }}).unwrap();
    let barrier = json!({"protocol":"1.0-draft.1","deliveryId":"delivery-fixture","rootId":root,"kind":"Feature","revision":"1",
        "approvedScopeDigest":plan_digest,"repositories":checkouts.iter().map(|(name,_,tree,base,_)|json!({"repositoryId":name,
            "pullRequestId":format!("pr-link-{name}"),"baseRef":"refs/heads/main","expectedHead":tree.head,
            "observedBase":{"format":"Sha1","hex":base},"reviewSource":"GitHubAutomated","mergeMethod":"MergeCommit",
            "evidence":evidence,"policyRevision":"1"})).collect::<Vec<_>>(),
        "dependencies":[{"before":"first","after":"second"}]});
    let service = |store: &Arc<dyn ApplicationStore>| {
        Arc::new(
            GitService::new(
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
                        path: std::fs::canonicalize(&owned).unwrap(),
                    }],
                    ..Default::default()
                },
            )
            .with_delivery_github(Some(Arc::new(GitHubService::new(
                store.clone(),
                client.clone(),
                h.ids.clone(),
                h.clock(),
            )))),
        )
    };
    let submit = |store: &Arc<dyn ApplicationStore>,
                  key: &str,
                  repo: usize,
                  supplied: Option<&Value>| {
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
        let mut frame = json!({"protocol":"1.0-draft.1","schemaRevision":17,"requestKey":key,"command":{"kind":"Integrate",
            "deliveryId":"delivery-fixture","expectedBarrierRevision":"1","repositoryId":checkouts[repo].0,"expectedHead":checkouts[repo].2.head}});
        if let Some(supplied) = supplied {
            frame["command"]["expectedBarrierRevision"] = supplied["revision"].clone();
            frame["command"]["barrier"] = supplied.clone();
        }
        match gateway
            .command(
                &user,
                "git.integrate",
                frame.to_string().as_bytes(),
                service(store),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => receipt.decision,
            other => panic!("{other:?}"),
        }
    };
    let accepted = |decision| match decision {
        DomainDecision::Accepted { operation_id } => operation_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        submit(&store, "dependent-before-predecessor", 1, Some(&barrier)),
        DomainDecision::Refused { .. }
    ));
    let stale_head = std::mem::replace(&mut wire.repos.lock().unwrap()[0].head, "f".repeat(40));
    let stale = accepted(submit(
        &store,
        "current-provider-head-must-match",
        0,
        Some(&barrier),
    ));
    let outcome = service(&store).execute(&begin(store.as_ref(), &stale));
    assert!(
        matches!(&outcome,EffectOutcome::Failed(reason) if reason.contains("delivery_pull_request")),
        "{outcome:?}"
    );
    wire.repos.lock().unwrap()[0].head = stale_head;
    let set_auto_merge = |expected: i64, enabled: bool| {
        let scope = ConfigScope::Repository {
            project: project.clone(),
            repository: id("second"),
        };
        write(store.as_ref(), move |tx| {
            let schema = devworks_core::settings::catalog::descriptor("delivery.autoMerge")
                .unwrap()
                .schema_ref();
            tx.configuration().append_revision(
                &scope,
                "settings",
                expected,
                1,
                json!({"values":{
                    "delivery.autoMerge":{"schema":schema,"value":enabled}
                }}),
            )?;
            Ok(())
        })
        .unwrap();
    };
    set_auto_merge(1, false);
    let disabled = accepted(submit(&store, "second-repository-auto-merge-off", 0, None));
    let outcome = service(&store).execute(&begin(store.as_ref(), &disabled));
    assert!(
        matches!(&outcome,EffectOutcome::Failed(reason) if reason.contains("auto_merge_disabled")),
        "{outcome:?}"
    );
    set_auto_merge(2, true);
    let gated = accepted(submit(
        &store,
        "other-repository-required-check-red",
        0,
        Some(&barrier),
    ));
    let outcome = service(&store).execute(&begin(store.as_ref(), &gated));
    assert!(
        matches!(&outcome,EffectOutcome::Failed(reason) if reason.contains("github_required_checks")),
        "{outcome:?} {:?}",
        wire.requests.lock().unwrap()
    );
    assert!(wire.submissions.lock().unwrap().is_empty());
    wire.repos.lock().unwrap()[1].green = true;
    let first = accepted(submit(&store, "ready-first-merge", 0, None));
    let first_record = begin(store.as_ref(), &first);
    assert!(matches!(
        service(&store).execute(&first_record),
        EffectOutcome::Unknown(_)
    ));
    assert!(matches!(
        submit(&store, "pending-first-holds-second", 1, None),
        DomainDecision::Refused { .. }
    ));
    assert_eq!(wire.submissions.lock().unwrap().as_slice(), ["first"]);
    drop(store);
    let store = h.open();
    assert!(matches!(
        service(&store).observe(&first_record),
        EffectOutcome::Unknown(_)
    ));
    wire.repos.lock().unwrap()[0].merged = true;
    let observed = service(&store).observe(&first_record);
    assert!(
        matches!(&observed,EffectOutcome::Completed(value) if value["result"]["value"]["featureComplete"]==false),
        "{observed:?}"
    );
    assert!(matches!(
        service(&store).observe(&first_record),
        EffectOutcome::Completed(_)
    ));
    assert_eq!(wire.submissions.lock().unwrap().as_slice(), ["first"]);
    mark_complete(store.as_ref(), &first, observed);
    let mut changed_order = barrier.clone();
    changed_order["revision"] = json!("2");
    changed_order["dependencies"] = json!([{"before":"second","after":"first"}]);
    assert!(matches!(
        submit(
            &store,
            "no-retroactive-predecessor",
            1,
            Some(&changed_order)
        ),
        DomainDecision::Refused { .. }
    ));
    let second = accepted(submit(&store, "ordered-second-merge", 1, None));
    let observed = service(&store).execute(&begin(store.as_ref(), &second));
    assert!(
        matches!(&observed,EffectOutcome::Completed(value) if value["result"]["value"]["featureComplete"]==true),
        "{observed:?}"
    );
    assert_eq!(
        wire.submissions.lock().unwrap().as_slice(),
        ["first", "second"]
    );
    assert_eq!(
        read(store.as_ref(), |s| s.work().work(&root.0))
            .unwrap()
            .completion
            .as_deref(),
        Some(VERIFIED)
    );
    mark_complete(store.as_ref(), &second, observed);
    let cleanup = |store: &Arc<dyn ApplicationStore>, key: &str, repo: usize| {
        let tree = &checkouts[repo].2;
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
        let frame = json!({"protocol":"1.0-draft.1","requestKey":key,"command":{"kind":"Cleanup",
            "worktreeId":tree.id,"expectedOwnershipRevision":"1","expectedHead":tree.head}});
        match gateway
            .command(
                &user,
                "git.cleanup",
                frame.to_string().as_bytes(),
                service(store),
            )
            .unwrap()
        {
            CommandReply::Recorded { receipt, .. } => receipt.decision,
            other => panic!("{other:?}"),
        }
    };
    let checkout = Path::new(&checkouts[0].2.path);
    std::fs::write(checkout.join("local-draft"), "uncommitted data").unwrap();
    let dirty = accepted(cleanup(&store, "dirty-cleanup-held", 0));
    let outcome = service(&store).execute(&begin(store.as_ref(), &dirty));
    assert!(
        matches!(&outcome,EffectOutcome::Failed(reason) if reason.contains("untracked")),
        "{outcome:?}"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("local-draft")).unwrap(),
        "uncommitted data"
    );
    std::fs::remove_file(checkout.join("local-draft")).unwrap();
    let automated = service(&store);
    automated.set_approval_control(Arc::new(
        devworks_core::services::runs::ControlService::new(h.ids.clone()),
    ));
    automated.schedule_completed_cleanup().unwrap();
    let pending = |repo: usize| {
        read(store.as_ref(), |s| {
            s.git().git_worktree(&checkouts[repo].2.id)
        })
        .unwrap()
        .pending_operation_id
        .expect("verified delivery schedules cleanup")
    };
    let clean = pending(0);
    let second_cleanup = pending(1);
    let outcome = service(&store).execute(&begin(store.as_ref(), &clean));
    assert!(
        matches!(outcome, EffectOutcome::Completed(_)),
        "{outcome:?}"
    );
    assert!(!checkout.exists());
    assert!(
        !fixture_git(&checkouts[0].1)
            .args(["show-ref", "--verify", "refs/heads/feature/owned"])
            .status()
            .unwrap()
            .success()
    );

    let second_record = begin(store.as_ref(), &second_cleanup);
    let source = &checkouts[1].1;
    let checkout = Path::new(&checkouts[1].2.path);
    let hash = devworks_core::fingerprint::sha256_hex(second_cleanup.0.as_str().as_bytes());
    let staged = owned.join(format!(".devworks-cleanup-{}", &hash[..24]));
    git(
        source,
        &[
            "worktree",
            "move",
            "--",
            checkout.to_str().unwrap(),
            staged.to_str().unwrap(),
        ],
    );
    drop(store);
    let store = h.open();
    std::fs::create_dir(checkout).unwrap();
    std::fs::write(checkout.join("replacement"), "other data").unwrap();
    assert!(matches!(
        service(&store).observe(&second_record),
        EffectOutcome::Unknown(_)
    ));
    assert_eq!(
        std::fs::read_to_string(checkout.join("replacement")).unwrap(),
        "other data"
    );
    assert!(staged.join("file").exists());
    std::fs::remove_file(checkout.join("replacement")).unwrap();
    std::fs::remove_dir(checkout).unwrap();
    assert!(matches!(
        service(&store).observe(&second_record),
        EffectOutcome::NotApplied(_)
    ));
    let outcome = service(&store).execute(&second_record);
    assert!(
        matches!(outcome, EffectOutcome::Completed(_)),
        "{outcome:?}"
    );
    assert!(!staged.exists() && !checkout.exists());
    assert!(
        !fixture_git(source)
            .args(["show-ref", "--verify", "refs/heads/feature/owned"])
            .status()
            .unwrap()
            .success()
    );
}
