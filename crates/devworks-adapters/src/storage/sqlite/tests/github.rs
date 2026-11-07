//! Source facts and genuine journal/clock transactions only. No remote effects or inference.
use super::*;
use devworks_core::protocol::{OperationState, ProjectId};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn read<T>(s: &SqliteStore, f: impl FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError>) -> T {
    read_snapshot(s, &ReadOptions::default(), f).unwrap()
}
fn setup(tx: &mut dyn WriteTransaction) -> Result<(), StorageError> {
    for name in ["a", "b", "c"] {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id(name)),
            name: name.into(),
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
    }
    for name in ["shared", "private"] {
        tx.projects().insert_repository(&StoredRepository {
            id: id(name),
            location: format!("/fixture/{name}"),
            name: name.into(),
            remote_url: Some(format!("https://github.com/org/{name}.git")),
            primary: false,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        })?;
    }
    for (p, r) in [("a", "shared"), ("b", "shared"), ("c", "private")] {
        tx.projects().set_members(&ProjectId(id(p)), &[id(r)])?;
    }
    tx.github().put_github_account(
        &StoredGithubAccount {
            id: id("account"),
            revision: 1,
            state: "Connected".into(),
            remote_user_id: Some("123".into()),
            login: Some("person".into()),
            credential_revision: 1,
            permission_revision: 1,
            granted_scopes: vec!["repo".into()],
            observed_at_ms: T0,
            last_error: None,
        },
        None,
    )?;
    for name in ["shared", "private"] {
        tx.github()
            .put_github_repository(&metadata(name, Some(name)), None)?;
        bind(tx, name, &format!("github-{name}"), 1, None)?;
    }
    Ok(())
}
fn bind(
    tx: &mut dyn WriteTransaction,
    native: &str,
    canonical: &str,
    revision: i64,
    expected: Option<i64>,
) -> Result<(), StorageError> {
    let n = tx.projects().repository(&id(native))?.unwrap();
    let remote = n.remote_url.clone().unwrap();
    let canonical = tx
        .github()
        .github_discovered_repository(&id(canonical))?
        .unwrap();
    let source = StoredGithubObservation {
        id: id(&format!("identity-{native}-{revision}")),
        repository_id: canonical.id.clone(),
        kind: "Repository".into(),
        subject_id: canonical.remote_repository_id.clone(),
        remote_revision: "actual-provider-metadata".into(),
        url: canonical.url.clone(),
        head: None,
        payload: json!({"requestedRemote":remote,"repository":{"node_id":canonical.remote_repository_id,"owner":{"login":canonical.owner},"name":canonical.name}}),
        source_updated_at_ms: Some(T0),
        observed_at_ms: T0,
    };
    let observed = tx.github().append_github_observation(&source)?;
    tx.github().put_github_repository_binding(
        &StoredGithubRepositoryBinding {
            repository_id: n.id,
            github_repository_id: canonical.id,
            revision,
            repository_revision: n.revision,
            registered_remote: remote,
            observation_id: observed.id,
            state: "Active".into(),
        },
        expected,
    )
}

fn metadata(name: &str, _registered: Option<&str>) -> StoredGithubRepository {
    StoredGithubRepository {
        id: id(&format!("github-{name}")),
        account_id: id("account"),
        revision: 1,
        remote_repository_id: format!("remote-{name}"),
        owner: "org".into(),
        name: name.into(),
        owner_kind: "Organization".into(),
        url: format!("https://github.com/org/{name}"),
        default_branch: Some("main".into()),
        permissions: json!({"pull":true,"push":true}),
        permission_revision: 1,
        observed_at_ms: T0,
    }
}
fn observed(repo: &str, n: i64, name: &str) -> StoredGithubObservation {
    StoredGithubObservation {
        id: id(name),
        repository_id: id(&format!("github-{repo}")),
        kind: "Issue".into(),
        subject_id: n.to_string(),
        remote_revision: format!("provider-update-{n}"),
        url: format!("https://github.com/org/{repo}/issues/{n}"),
        head: None,
        payload: json!({"number":n,"title":"A report","body":"source body","state":"open"}),
        source_updated_at_ms: Some(T0),
        observed_at_ms: T0,
    }
}
fn incoming(
    tx: &mut dyn WriteTransaction,
    repo: &str,
    n: i64,
    name: &str,
) -> Result<StoredGithubIssueIntake, StorageError> {
    let o = tx
        .github()
        .append_github_observation(&observed(repo, n, name))?;
    let r = StoredGithubIssueIntake {
        key: GithubIssueKey {
            repository_id: id(&format!("github-{repo}")),
            number: n,
        },
        revision: 1,
        state: "Queued".into(),
        observation_id: o.id.clone(),
        remote_revision: o.id.as_str().into(),
        project_id: None,
        work_id: None,
        operation_id: None,
        last_triaged_revision: None,
    };
    tx.github().put_github_issue(&r, None)?;
    Ok(r)
}
fn triage(
    tx: &mut dyn WriteTransaction,
    r: &StoredGithubIssueIntake,
    name: &str,
) -> Result<OperationId, StorageError> {
    let op = OperationId(id(name));
    tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:"github.triage".into(),schema_version:1,args_digest:"actual-intent".into(),payload:json!({"repositoryId":r.key.repository_id,"issueNumber":r.key.number,"sourceRevision":r.remote_revision})})?;
    Ok(op)
}
fn scope(project: Option<&str>) -> GithubScope {
    GithubScope {
        repositories: vec![id("shared"), id("private")],
        project_id: project.map(|p| ProjectId(id(p))),
    }
}
fn task(tx: &mut dyn WriteTransaction, name: &str, project: &str) -> Result<RootId, StorageError> {
    let w = StoredWork {
        id: id(name),
        project_id: ProjectId(id(project)),
        kind: TASK.into(),
        parent_feature_id: None,
        status_id: None,
        title: name.into(),
        priority: 0,
        manual_order: 0,
        revision: 1,
        scope_revision: 1,
        created_at_ms: T0,
        updated_at_ms: T0,
        created_by: None,
        updated_by: None,
        search_key: name.into(),
        caller_ref: None,
        completion: None,
        brief: json!({"description":"actual local brief"}),
    };
    tx.work().insert_work(&w)?;
    Ok(RootId(w.id))
}
#[test]
fn github_discovery_scoped_facts_full_bodies_and_endpoint_cursors_survive_reopen() {
    let h = Harness::new();
    let store = h.store();
    let (visible, intake) = run_write(&store, WriteOptions::default(), |tx| {
        setup(tx)?;
        tx.github()
            .put_github_repository(&metadata("not-registered", None), None)?;
        let page = tx
            .github()
            .github_account_repositories(&id("account"), Some("ORG"), None, 1)?;
        assert_eq!(page.len(), 1);
        assert_eq!(
            tx.github()
                .github_account_repositories(&id("account"), Some("org"), Some(&page[0].id), 500)?
                .len(),
            2
        );
        assert_eq!(
            tx.github()
                .github_discovered_repository(&id("github-not-registered"))?
                .unwrap()
                .remote_repository_id,
            "remote-not-registered"
        );
        assert!(
            tx.projects()
                .repository(&id("github-not-registered"))?
                .is_none(),
            "remote identity is never an invented native checkout"
        );
        tx.github()
            .append_github_observation(&observed("private", 9, "a-hidden"))?;
        let mut o = observed("shared", 1, "z-visible");
        o.payload["body"] = json!(format!("{}\0exact", "λ".repeat(65536)));
        let visible = tx.github().append_github_observation(&o)?;
        let mut duplicate = o.clone();
        duplicate.id = id("duplicate-poll");
        duplicate.observed_at_ms += 1;
        assert_eq!(tx.github().append_github_observation(&duplicate)?, visible);
        let f = GithubObservationFilter {
            scope: scope(Some("a")),
            kinds: vec!["Issue".into()],
            ..Default::default()
        };
        let records = tx.github().github_observations(&f, None, 1)?;
        assert_eq!(records, vec![visible.clone()]);
        assert!(
            tx.github()
                .github_observation(&id("a-hidden"), &scope(Some("a")))?
                .is_none()
        );
        let intake = StoredGithubIssueIntake {
            key: GithubIssueKey {
                repository_id: id("github-shared"),
                number: 1,
            },
            revision: 1,
            state: "Queued".into(),
            observation_id: visible.id.clone(),
            remote_revision: visible.id.as_str().into(),
            project_id: None,
            work_id: None,
            operation_id: None,
            last_triaged_revision: None,
        };
        tx.github().put_github_issue(&intake, None)?;
        assert!(tx.github().put_github_issue(&intake, None).is_err());
        assert!(
            tx.github()
                .github_issue(&intake.key, &scope(Some("a")))?
                .is_some()
        );
        assert!(
            tx.github()
                .github_issue(&intake.key, &scope(Some("b")))?
                .is_some()
        );
        for endpoint in ["issues?state=all", "issues/1/comments"] {
            let c = StoredGithubPollCursor {
                repository_id: id("github-shared"),
                endpoint: endpoint.into(),
                revision: 1,
                etag: Some(format!("etag-{endpoint}")),
                last_modified: None,
                next_page: Some("https://api.github.com/repos/org/shared/issues?page=2".into()),
                watermark: Some(json!({"updatedAt":T0,"id":"stable-tie"})),
                bootstrap_complete: false,
                next_poll_at_ms: T0 + 1000,
                last_polled_at_ms: Some(T0),
                failure_count: 2,
                last_error: Some("rate limited".into()),
            };
            tx.github().put_github_poll_cursor(&c, None)?;
            assert!(tx.github().put_github_poll_cursor(&c, None).is_err());
        }
        Ok((visible, intake))
    })
    .unwrap();
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r
            .github()
            .github_observation(&visible.id, &scope(Some("a"))))
        .unwrap(),
        visible
    );
    assert_eq!(
        read(&store, |r| r
            .github()
            .github_issue(&intake.key, &scope(Some("b"))))
        .unwrap()
        .project_id,
        None
    );
    assert_eq!(
        read(&store, |r| r.github().github_poll_cursor(
            &id("github-shared"),
            "issues/1/comments"
        ))
        .unwrap()
        .failure_count,
        2
    );
    assert!(
        read(&store, |r| r.github().github_poll_cursor(
            &id("github-shared"),
            "issues?state=open"
        ))
        .is_none()
    );
    assert!(
        read_snapshot(&store, &ReadOptions::default(), |r| r
            .github()
            .github_observations(
                &GithubObservationFilter {
                    scope: scope(None),
                    ..Default::default()
                },
                None,
                501
            ))
        .is_err()
    );
}
#[test]
fn one_global_clock_claim_lost_ack_and_restart_never_accumulate_burst_credits() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    let (rows, ops) = run_write(&*store, WriteOptions::default(), |tx| {
        setup(tx)?;
        let mut rows = vec![];
        let mut ops = vec![];
        for (n, repo) in [(1, "shared"), (2, "private"), (3, "shared")] {
            let r = incoming(tx, repo, n, &format!("source-{n}"))?;
            ops.push(triage(tx, &r, &format!("triage-{n}"))?);
            rows.push(r);
        }
        Ok((rows, ops))
    })
    .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let joins = (0..2)
        .map(|n| {
            let s = store.clone();
            let gate = barrier.clone();
            let row = rows[n].clone();
            let op = ops[n].clone();
            std::thread::spawn(move || {
                gate.wait();
                run_write(&*s, WriteOptions::default(), move |tx| {
                    tx.github().admit_github_issue(&row.key, 1, 1, &op)
                })
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let results = joins
        .into_iter()
        .map(|j| j.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|r| matches!(r, Ok(Some(_)))).count(),
        1
    );
    let winner = results
        .iter()
        .find_map(|r| r.as_ref().ok().and_then(Option::as_ref))
        .unwrap()
        .clone();
    let saved = winner.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        let replay = tx
            .github()
            .admit_github_issue(&saved.issue.key, 1, 1, &saved.operation.id)?
            .unwrap();
        assert_eq!(replay.clock, saved.clock);
        let a = tx
            .operations()
            .begin_attempt(&saved.operation.id, 1, AttemptBinding::default())?;
        tx.operations().transition(
            &saved.operation.id,
            2,
            Some(&a.fence()),
            OperationState::Dispatching,
            None,
        )?;
        tx.operations().transition(
            &saved.operation.id,
            3,
            Some(&a.fence()),
            OperationState::OutcomeUnknown,
            None,
        )?;
        Ok(())
    })
    .unwrap();
    let initial = read(&store, |r| r.github().github_intake_clock());
    assert_eq!(initial.next_admission_at_ms, T0 + 1_800_000);
    h.clock.0.store(T0 + 18_000_000, Ordering::SeqCst);
    let waiting = rows
        .iter()
        .zip(ops.iter())
        .find(|(_, op)| **op != winner.operation.id)
        .map(|(r, o)| (r.clone(), o.clone()))
        .unwrap();
    let w = waiting.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        let c = tx.github().github_intake_clock()?;
        let admitted = tx
            .github()
            .admit_github_issue(&w.0.key, 1, c.revision, &w.1)?
            .unwrap();
        assert_eq!(admitted.clock.next_admission_at_ms, tx.now_ms() + 1_800_000);
        Ok(())
    })
    .unwrap();
    let third = rows[2].clone();
    let third_op = ops[2].clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        let c = tx.github().github_intake_clock()?;
        assert!(
            tx.github()
                .admit_github_issue(&third.key, 1, c.revision, &third_op)?
                .is_none()
        );
        assert!(
            tx.github()
                .set_github_intake_interval(c.revision, i64::MAX)
                .is_err()
        );
        assert_eq!(tx.github().github_intake_clock()?, c);
        Ok(())
    })
    .unwrap();
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r.operations().operation(&winner.operation.id))
            .unwrap()
            .state,
        OperationState::OutcomeUnknown
    );
    let clock = read(&store, |r| r.github().github_intake_clock());
    assert_eq!(clock.last_admitted_at_ms, Some(T0 + 18_000_000));
}
fn resolution_ops(
    tx: &mut dyn WriteTransaction,
    k: &GithubIssueKey,
    d: &str,
    suffix: &str,
) -> Result<(OperationId, OperationId), StorageError> {
    let mut ids = vec![];
    for kind in ["comment", "close"] {
        let op = OperationId(id(&format!("{suffix}-{kind}")));
        tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:format!("github.resolution.{kind}"),schema_version:1,args_digest:d.into(),payload:json!({"repositoryId":k.repository_id,"issueNumber":k.number,"completionDigest":d})})?;
        ids.push(op);
    }
    Ok((ids.remove(0), ids.remove(0)))
}
#[test]
fn project_obligations_and_immutable_completion_witnesses_use_only_actual_effect_journal() {
    let h = Harness::new();
    let store = h.store();
    let binding = run_write(&store, WriteOptions::default(), |tx| {
        setup(tx)?;
        let issue = incoming(tx, "shared", 42, "issue-snapshot")?;
        let a = task(tx, "work-a", "a")?;
        let b = task(tx, "work-b", "b")?;
        let c = task(tx, "work-c", "c")?;
        assert!(tx.github().link_github_work(&issue.key, &c, 1).is_err());
        tx.github().link_github_work(&issue.key, &a, 1)?;
        tx.github().link_github_work(&issue.key, &b, 1)?;
        let generation = tx.github().github_obligation_revision(&issue.key)?;
        tx.github().link_github_work(&issue.key, &a, 1)?;
        assert_eq!(
            tx.github().github_obligation_revision(&issue.key)?,
            generation
        );
        assert_eq!(
            tx.github()
                .github_work_links(&issue.key, Some(&ProjectId(id("a"))), None, 1)?,
            vec![GithubWorkLink {
                work_id: a.clone(),
                project_id: ProjectId(id("a"))
            }]
        );
        let mut routed = issue.clone();
        routed.revision = 2;
        routed.state = "Working".into();
        routed.project_id = Some(ProjectId(id("b")));
        routed.work_id = Some(b.clone());
        tx.github().put_github_issue(&routed, Some(1))?;
        assert!(
            tx.github()
                .github_issue(&issue.key, &scope(Some("a")))?
                .is_none(),
            "repository-readable remote facts do not disclose another project's local work identity"
        );
        assert!(
            tx.github()
                .github_observation(&issue.observation_id, &scope(Some("a")))?
                .is_some()
        );
        let digest = "a".repeat(64);
        let (comment, close) = resolution_ops(tx, &issue.key, &digest, "first")?;
        let binding = StoredGithubResolution {
            id: id("resolution"),
            key: issue.key.clone(),
            state: "Draft".into(),
            completion_digest: digest,
            source_revision: issue.remote_revision.clone(),
            obligation_revision: generation,
            summary_digest: "b".repeat(64),
            comment_operation_id: comment,
            close_operation_id: close,
        };
        tx.github().insert_github_resolution(&binding)?;
        let witness = StoredGithubResolutionWitness {
            resolution_id: binding.id.clone(),
            work_id: a,
            project_id: ProjectId(id("a")),
            work_revision: 1,
            evidence: json!({"source":"actual-work-brief","criteria":[]}),
        };
        tx.github().insert_github_resolution_witness(&witness)?;
        assert!(
            tx.github().seal_github_resolution(&binding.id, 2).is_err(),
            "one project's completion cannot publish for two obligations"
        );
        let mut other = witness.clone();
        other.work_id = b;
        assert!(
            tx.github()
                .insert_github_resolution_witness(&other)
                .is_err()
        );
        other.project_id = ProjectId(id("b"));
        tx.github().insert_github_resolution_witness(&other)?;
        let sealed = tx.github().seal_github_resolution(&binding.id, 2)?;
        assert_eq!(sealed.state, "Sealed");
        assert_eq!(
            tx.operations()
                .operation(&sealed.comment_operation_id)?
                .unwrap()
                .state,
            OperationState::IntentRecorded
        );
        let mut changed = witness;
        changed.evidence = json!({"forged":"replacement"});
        assert!(
            tx.github()
                .insert_github_resolution_witness(&changed)
                .is_err()
        );
        for (op, end) in [
            (
                &binding.comment_operation_id,
                OperationState::ObservedComplete,
            ),
            (&binding.close_operation_id, OperationState::OutcomeUnknown),
        ] {
            let a = tx
                .operations()
                .begin_attempt(op, 1, AttemptBinding::default())?;
            tx.operations().transition(
                op,
                2,
                Some(&a.fence()),
                OperationState::Dispatching,
                None,
            )?;
            tx.operations()
                .transition(op, 3, Some(&a.fence()), end, None)?;
        }
        assert_eq!(
            tx.github().insert_github_resolution(&binding)?.id,
            binding.id,
            "lost acknowledgment resolves the same binding and real operations"
        );
        let digest = "c".repeat(64);
        let (comment, close) = resolution_ops(tx, &issue.key, &digest, "second")?;
        let conflicting = StoredGithubResolution {
            id: id("another-resolution"),
            completion_digest: digest,
            comment_operation_id: comment,
            close_operation_id: close,
            ..binding.clone()
        };
        assert!(
            matches!(
                tx.github().insert_github_resolution(&conflicting),
                Err(StorageError::Busy)
            ),
            "uncertain close retains exclusive source publication ownership"
        );
        Ok(binding)
    })
    .unwrap();
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r.github().github_resolution(&binding.id))
            .unwrap()
            .state,
        "Sealed"
    );
    assert_eq!(
        read(&store, |r| r
            .operations()
            .operation(&binding.comment_operation_id))
        .unwrap()
        .state,
        OperationState::ObservedComplete
    );
    assert_eq!(
        read(&store, |r| r
            .operations()
            .operation(&binding.close_operation_id))
        .unwrap()
        .state,
        OperationState::OutcomeUnknown
    );
    assert_eq!(
        read(&store, |r| r.github().github_resolution_witnesses(
            &binding.id,
            Some(&ProjectId(id("a"))),
            None,
            500
        ))
        .len(),
        1
    );
}
#[test]
fn two_checkout_bindings_preserve_one_canonical_intake_across_detach_rebind_and_registry_deletion()
{
    let h = Harness::new();
    let store = h.store();
    let (old, new) = run_write(&store, WriteOptions::default(), |tx| {
        setup(tx)?;
        let second = StoredRepository {
            id: id("second-checkout"),
            location: "/different/checkout".into(),
            name: "Second checkout".into(),
            remote_url: Some("git@github.com:org/shared.git".into()),
            primary: false,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        };
        tx.projects().insert_repository(&second)?;
        tx.projects()
            .set_members(&ProjectId(id("b")), std::slice::from_ref(&second.id))?;
        bind(tx, "second-checkout", "github-shared", 1, None)?;
        let old = incoming(tx, "shared", 77, "canonical-source")?;
        let b = GithubScope {
            repositories: vec![second.id.clone()],
            project_id: Some(ProjectId(id("b"))),
        };
        assert_eq!(tx.github().github_issue(&old.key, &b)?.unwrap(), old);
        assert_eq!(
            tx.github().github_repository(&second.id)?.unwrap().id,
            id("github-shared")
        );
        let all = GithubIntakeFilter {
            scope: GithubScope {
                repositories: vec![id("shared"), second.id.clone()],
                project_id: None,
            },
            states: vec![],
        };
        assert_eq!(
            tx.github().github_issues(&all, None, 500)?.len(),
            1,
            "aliases never duplicate a source row before paging"
        );
        let duplicate =
            tx.github()
                .append_github_observation(&observed("shared", 77, "another-alias-poll"))?;
        assert_eq!(duplicate.id, old.observation_id);
        let mut detached = tx
            .github()
            .github_repository_binding(&id("shared"))?
            .unwrap();
        detached.revision = 2;
        detached.state = "Detached".into();
        tx.github()
            .put_github_repository_binding(&detached, Some(1))?;
        assert!(
            tx.github()
                .github_issue(&old.key, &scope(Some("a")))?
                .is_none()
        );
        assert!(tx.github().github_issue(&old.key, &b)?.is_some());
        let mut canonical = metadata("shared", None);
        canonical.id = id("github-recreated");
        canonical.remote_repository_id = "new-real-server-node".into();
        tx.github().put_github_repository(&canonical, None)?;
        bind(tx, "shared", "github-recreated", 3, Some(2))?;
        assert!(
            tx.github()
                .github_issue(&old.key, &scope(Some("a")))?
                .is_none(),
            "rebind cannot grant the old source scope"
        );
        assert!(tx.github().github_issue(&old.key, &b)?.is_some());
        let history = tx
            .github()
            .github_repository_binding_history(&id("shared"), None, 1)?;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].github_repository_id, id("github-shared"));
        assert_eq!(
            tx.github()
                .github_repository_binding_history(&id("shared"), Some(1), 500)?
                .len(),
            2
        );
        let mut source = observed("shared", 77, "new-server-source");
        source.repository_id = canonical.id.clone();
        source.payload["body"] = json!("different actual repository incarnation");
        let source = tx.github().append_github_observation(&source)?;
        let new = StoredGithubIssueIntake {
            key: GithubIssueKey {
                repository_id: canonical.id,
                number: 77,
            },
            observation_id: source.id.clone(),
            remote_revision: source.id.as_str().into(),
            ..old.clone()
        };
        tx.github().put_github_issue(&new, None)?;
        assert!(
            tx.github()
                .github_issue(&new.key, &scope(Some("a")))?
                .is_some()
        );
        assert!(tx.github().github_issue(&new.key, &b)?.is_none());
        assert_eq!(
            tx.github()
                .github_repository_by_remote_id("remote-shared")?
                .unwrap()
                .id,
            old.key.repository_id
        );
        let mut forged = tx.github().github_repository_binding(&second.id)?.unwrap();
        forged.revision = 2;
        forged.github_repository_id = id("github-private");
        assert!(
            tx.github()
                .put_github_repository_binding(&forged, Some(1))
                .is_err(),
            "a caller's canonical ID cannot replace the actual server observation"
        );
        assert_eq!(
            tx.github()
                .github_repository_binding(&second.id)?
                .unwrap()
                .revision,
            1
        );
        Ok((old, new))
    })
    .unwrap();
    // Current registration lifecycle is independently authoritative; a retained Active binding
    // cannot keep a deleted checkout's former project scope alive.
    h.raw().execute("UPDATE repository_details SET lifecycle='Deleted' WHERE repository_id='second-checkout'",[]).unwrap();
    let b = GithubScope {
        repositories: vec![id("second-checkout")],
        project_id: Some(ProjectId(id("b"))),
    };
    assert!(read(&store, |r| r.github().github_issue(&old.key, &b)).is_none());
    assert_eq!(
        read(&store, |r| r
            .github()
            .github_repository_binding(&id("second-checkout")))
        .unwrap()
        .state,
        "Stale"
    );
    assert_eq!(
        read(&store, |r| r.github().github_repository_binding_history(
            &id("second-checkout"),
            None,
            1
        ))[0]
            .state,
        "Active"
    );
    assert!(
        read(&store, |r| r
            .github()
            .github_issue(&new.key, &scope(Some("a"))))
        .is_some()
    );
    let raw = h.raw();
    let canonical: i64 = raw
        .query_row(
            "SELECT count(*) FROM integration_records WHERE kind='GithubIssueIntake'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let legacy: i64 = raw
        .query_row("SELECT count(*) FROM github_issue_intake", [], |r| r.get(0))
        .unwrap();
    let fake: i64 = raw
        .query_row(
            "SELECT count(*) FROM repositories WHERE id LIKE 'github-%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((canonical, legacy, fake), (2, 0, 0));
    drop(raw);
    drop(store);
    let store = h.store();
    assert_eq!(
        read(&store, |r| r.github().github_repository_binding_history(
            &id("shared"),
            None,
            500
        ))
        .len(),
        3
    );
}
#[test]
fn provider_etag_is_not_snapshot_identity_and_direct_state_updates_cannot_bypass_pacing() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        setup(tx)?;
        let original = incoming(tx, "shared", 5, "old-snapshot")?;
        let op = triage(tx, &original, "old-snapshot-triage")?;
        let mut updated = observed("shared", 5, "new-snapshot");
        updated.payload["body"] = json!("changed body under the same provider timestamp/etag");
        let updated = tx.github().append_github_observation(&updated)?;
        assert_eq!(
            updated.remote_revision,
            observed("shared", 5, "unused").remote_revision
        );
        let mut row = original.clone();
        row.revision = 2;
        row.observation_id = updated.id.clone();
        row.remote_revision = updated.id.as_str().into();
        tx.github().put_github_issue(&row, Some(1))?;
        assert!(tx.github().admit_github_issue(&row.key, 2, 1, &op).is_err());
        assert_eq!(tx.github().github_intake_clock()?.revision, 1);
        let mut bypass = row;
        bypass.revision = 3;
        bypass.state = "Triaging".into();
        bypass.operation_id = Some(op);
        assert!(tx.github().put_github_issue(&bypass, Some(2)).is_err());
        assert_eq!(tx.github().github_intake_clock()?.last_admitted_at_ms, None);
        Ok(())
    })
    .unwrap();
}
fn adjudication_artifact(
    tx: &mut dyn WriteTransaction,
    r: &StoredGithubObligationRetirement,
) -> Result<serde_json::Value, StorageError> {
    let bytes=serde_json::to_vec(&json!({"reason":r.reason,"workId":r.work_id,"workRevision":r.work_revision,"observationId":r.observation_id,"source":"recorded controlled fixture"})).unwrap();
    let digest = devworks_core::fingerprint::sha256_hex(&bytes);
    let blob = tx.artifacts().put_blob(
        &digest,
        bytes.len() as i64,
        "adjudication-blob",
        "blobs/adjudication",
    )?;
    let actor = tx.store().host_actor.clone();
    let provenance = json!({"principalKind":"Host","producedBy":actor,"derived":{"kind":"GithubObligationAdjudication","repositoryId":r.key.repository_id,"issueNumber":r.key.number,"workId":r.work_id,"workRevision":r.work_revision,"obligationRevision":r.obligation_revision,"operationId":r.operation_id,"observationId":r.observation_id,"reason":r.reason,"actorId":r.actor_id}});
    tx.artifacts().insert_artifact(&StoredArtifact {
        id: id("adjudication"),
        origin_project_id: Some(r.project_id.clone()),
        origin_work_id: Some(r.work_id.clone()),
        title: "Recorded adjudication".into(),
        revision: 1,
        scope: ArtifactScope::Project(r.project_id.clone()),
        owner: Some(actor.clone()),
        retention: ArtifactRetention::Explicit,
        scope_revision: 1,
        reference_revision: 1,
        current_version_id: None,
        state: ArtifactState::Live,
        created_at_ms: T0,
        provenance,
    })?;
    tx.artifacts().insert_version(&StoredVersion {
        id: id("adjudication-version"),
        artifact_id: id("adjudication"),
        version_no: 1,
        blob_digest: digest.clone(),
        byte_count: bytes.len() as i64,
        blob_generation: blob.generation,
        media_type: "application/json".into(),
        created_at_ms: T0,
        deleted_at_ms: None,
        source: json!({"producedBy":actor,"principalKind":"Host","source":{"operationId":r.operation_id,"actorId":r.actor_id,"observationId":r.observation_id}}),
    })?;
    Ok(
        json!({"artifactId":"adjudication","versionId":"adjudication-version","sha256":digest,"byteLength":bytes.len().to_string(),"mediaType":"application/json"}),
    )
}
#[test]
fn adjudicated_obligation_retirement_preserves_history_and_invalidates_old_completion_without_mutating_work()
 {
    let h = Harness::new();
    let store = h.store();
    run_write(&store,WriteOptions::default(),|tx|{
  setup(tx)?;let issue=incoming(tx,"shared",88,"retirement-source")?;let a=task(tx,"misrouted-work","a")?;let b=task(tx,"right-work","b")?;tx.github().link_github_work(&issue.key,&a,1)?;tx.github().link_github_work(&issue.key,&b,1)?;let generation=tx.github().github_obligation_revision(&issue.key)?;
  let digest="e".repeat(64);let(comment,close)=resolution_ops(tx,&issue.key,&digest,"old-scope")?;let completion=StoredGithubResolution{id:id("old-completion"),key:issue.key.clone(),state:"Draft".into(),completion_digest:digest,source_revision:issue.remote_revision.clone(),obligation_revision:generation,summary_digest:"f".repeat(64),comment_operation_id:comment,close_operation_id:close};tx.github().insert_github_resolution(&completion)?;
  let original_actor=devworks_core::protocol::ActorId(id("original-agent"));tx.actors().register(&original_actor,devworks_core::context::PrincipalKind::Agent,"Original agent")?;
  let op=OperationId(id("actual-adjudication"));tx.operations().record_intent(NewOperation{id:op.clone(),root_id:None,assignment_id:None,capability:"github.triage".into(),schema_version:1,args_digest:"adjudication".into(),payload:json!({"repositoryId":issue.key.repository_id,"issueNumber":issue.key.number,"sourceRevision":issue.remote_revision,"actorId":original_actor})})?;let attempt=tx.operations().begin_attempt(&op,1,AttemptBinding::default())?;tx.operations().transition(&op,2,Some(&attempt.fence()),OperationState::Dispatching,None)?;tx.operations().transition(&op,3,Some(&attempt.fence()),OperationState::ObservedComplete,Some(json!({"reason":"misrouted","source":"recorded controlled fixture"})))?;
  let mut witness=StoredGithubObligationRetirement{id:id("retirement"),key:issue.key.clone(),work_id:a.clone(),project_id:ProjectId(id("a")),work_revision:1,obligation_revision:generation,reason:GithubObligationRetirementReason::Misrouted,operation_id:op,actor_id:original_actor,observation_id:issue.observation_id,adjudication_ref:json!({}),created_at_ms:T0};
  assert!(tx.github().retire_github_work_link(&witness).is_err(),"a caller reason flag and operation alone do not supply an actual qualified adjudication witness");witness.adjudication_ref=adjudication_artifact(tx,&witness)?;
  let before=tx.work().work(&a.0)?.unwrap();tx.github().retire_github_work_link(&witness)?;tx.github().retire_github_work_link(&witness)?;
  assert_eq!(tx.github().github_adjudication_owner(&id("adjudication"),Some(&id("adjudication-version")))?.unwrap().actor_id,witness.actor_id);
  assert_ne!(witness.actor_id,tx.artifacts().artifact(&id("adjudication"))?.unwrap().owner.unwrap(),"Host owns proof while the actual Agent remains attributed");
  assert_eq!(tx.work().work(&a.0)?.unwrap(),before,"retirement neither deletes nor closes work or changes controls");
  assert_eq!(tx.github().github_work_links(&issue.key,None,None,500)?.len(),1);assert_eq!(tx.github().github_related_work(&issue.key,None,None,500)?.len(),2);assert_eq!(tx.github().github_obligation_revision(&issue.key)?,generation+1);assert!(tx.github().seal_github_resolution(&completion.id,1).is_err());
  assert_eq!(tx.github().github_obligation_retirements(&issue.key,Some(&ProjectId(id("a"))),None,500)?,vec![witness.clone()]);assert!(tx.github().github_obligation_retirements(&issue.key,Some(&ProjectId(id("b"))),None,500)?.is_empty());
  let mut stale=witness;stale.reason=GithubObligationRetirementReason::UnstartedUnreproducible;assert!(tx.github().retire_github_work_link(&stale).is_err());Ok(())
 }).unwrap();
}
