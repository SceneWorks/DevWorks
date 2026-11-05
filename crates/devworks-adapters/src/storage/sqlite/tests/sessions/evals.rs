//! Recorded-work grading through native SQLite, host transcript writes and the gateway.
use super::*;
use devworks_core::protocol::records::{
    EvalOutcome, EvalReport, EvalReportPage, EvalRevisionContext, EvalSuiteSnapshot,
};
use devworks_core::services::evals::{
    EvalService,
    attribution::{REVISION_CATEGORIES, revision_manifest},
};

fn fixture() -> Fx {
    let mut fx = Fx::new();
    enable(&mut fx);
    fx.seed_session("session-a", "agent-a", Some("root-a"));
    fx
}
fn enable(fx: &mut Fx) {
    let service = Arc::new(EvalService::new(fx.h.ids.clone(), fx.h.clock.clone()));
    fx.cases = std::mem::replace(&mut fx.cases, UseCases::new()).with_evals(service);
}
fn suite(checks: Vec<Value>) -> EvalSuiteSnapshot {
    serde_json::from_value(
        json!({"id":"suite-a","revision":"1","name":"Recorded checks","checks":checks}),
    )
    .unwrap()
}
fn contains(name: &str, kind: &str, needle: &str) -> Value {
    json!({"id":name,"ruleRef":"recorded acceptance evidence","measurement":"Proxy",
        "predicate":{"kind":"TranscriptContains","entryKind":kind,"text":needle}})
}
fn tool_check() -> Value {
    json!({"id":"tools","ruleRef":"recorded tool completion","measurement":"Proxy",
        "predicate":{"kind":"ToolSucceeded","operation":"*"}})
}
fn grade(fx: &Fx, suite: &EvalSuiteSnapshot) -> EvalReport {
    match applied(fx.command(
        &fx.user(),
        "evals.grade",
        json!({"sessionId":"session-a","suite":suite}),
    )) {
        OperationResult::EvalsGrade { value } => value,
        other => panic!("unexpected grade: {other:?}"),
    }
}
fn results(
    fx: &Fx,
    ctx: &InvocationContext,
    session: &str,
    cursor: Option<&str>,
) -> DomainQueryReply {
    let mut page = json!({"limit":1});
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    fx.query(
        ctx,
        "evals.results",
        json!({"sessionId":session,"page":page}),
    )
}
fn page(reply: DomainQueryReply) -> EvalReportPage {
    match result(reply) {
        OperationResult::EvalsResults { value } => value,
        other => panic!("{other:?}"),
    }
}
fn assistant(value: &str, interrupted: bool) -> Value {
    json!({"kind":"AssistantAttempt","requestId":"request-a","attemptId":"attempt-a",
        "visibleText":value,"interrupted":interrupted})
}
fn append_values(fx: &Fx, name: &str, bodies: Vec<Value>, sources: Vec<ContextSourceInput>) {
    let first = fx.session("session-a").next_sequence;
    let entries = bodies
        .into_iter()
        .enumerate()
        .map(|(i, body)| entry(&format!("{name}-{i}"), first + i as i64, body))
        .collect();
    let mut batch = SessionBatch::entries(tbatch(name, "session-a", 0, first, entries));
    batch.context_sources = sources;
    accepted(fx.append(&fx.worker("agent-a", "session-a"), batch));
}
fn manifest(revision: i64) -> Vec<ContextSourceInput> {
    let mut sources: Vec<_> = REVISION_CATEGORIES
        .iter()
        .map(|kind| ContextSourceInput {
            source_id: id(&format!("none-{}", kind.replace('.', "-"))),
            kind: (*kind).into(),
            source_revision: 1,
            artifact: None,
            token_estimate: 0,
            disposition: SourceDisposition::Included,
        })
        .collect();
    sources[0].source_id = id("effective-skill");
    sources[0].source_revision = revision;
    sources.push(revision_manifest(&sources).unwrap());
    sources
}
fn outcome(report: &EvalReport, expected: EvalOutcome) -> bool {
    report.runs.0.iter().flat_map(|r| &r.instances.0).any(|i| {
        serde_json::to_value(&i.outcome).unwrap() == serde_json::to_value(&expected).unwrap()
    })
}
/// Capture entire stored rows in all owned work/repository/Knowledge tables, including empty
/// tables. Evaluation receipts/reports/history are intentionally outside this invariant.
fn protected_rows(fx: &Fx) -> Vec<(String, Vec<String>)> {
    fx.store.with_writer_connection(|c| {
        let mut tables = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND (name LIKE 'knowledge_%' OR name LIKE 'work_%' OR name LIKE 'project_%' OR name='projects' OR name='repositories' OR name LIKE 'git_%') ORDER BY name").unwrap();
        let names: Vec<String> = tables.query_map([],|r|r.get(0)).unwrap().map(Result::unwrap).collect();
        let mut snapshots = Vec::new();
        for name in names {
            let mut statement = c.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
            let count = statement.column_count();
            let mut rows = statement.query_map([],|r| {
                let mut cells=Vec::new();
                for n in 0..count {cells.push(format!("{:?}",r.get_ref(n)?));}
                Ok(cells.join("|"))
            }).unwrap().map(Result::unwrap).collect::<Vec<_>>();
            rows.sort();snapshots.push((name,rows));
        }
        Ok(snapshots)
    }).unwrap()
}

#[test]
fn recorded_grades_persist_without_work_repository_or_knowledge_mutations() {
    let fx = fixture();
    append_values(
        &fx,
        "observed",
        vec![
            assistant("verified", false),
            json!({"kind":"ToolCallRecorded","toolCall":call("success",0)}),
            json!({"kind":"ToolResponseRecorded","call":key("success",0),"outcome":{"kind":"Completed","artifacts":[]}}),
        ],
        manifest(1),
    );
    let before = protected_rows(&fx);
    let measurement = suite(vec![
        contains("text", "AssistantAttempt", "verified"),
        tool_check(),
    ]);
    let report = grade(&fx, &measurement);
    assert!(matches!(report.outcome, EvalOutcome::Passed));
    assert_eq!(report.coverage.eligible.get(), 2);
    assert_eq!(report.coverage.graded.get(), 2);
    assert!(
        report.runs.0[0]
            .instances
            .0
            .iter()
            .all(|i| matches!(i.revision, EvalRevisionContext::Exact { .. }))
    );
    assert_eq!(protected_rows(&fx), before);
    let again = grade(&fx, &measurement);
    assert_eq!(report.id, again.id);
    assert_eq!(
        page(results(&fx, &fx.user(), "session-a", None))
            .items
            .0
            .len(),
        1
    );
    assert_eq!(protected_rows(&fx), before);
    let mut fx = fx.restart();
    enable(&mut fx);
    let persisted = page(results(&fx, &fx.user(), "session-a", None));
    assert_eq!(
        serde_json::to_value(&persisted.items.0[0]).unwrap(),
        serde_json::to_value(&report).unwrap()
    );
    assert_eq!(protected_rows(&fx), before);
}

#[test]
fn each_step_uses_its_recorded_revision_and_unmanifested_steps_are_explicitly_mixed() {
    let fx = fixture();
    append_values(&fx, "missing", vec![assistant("ok", false)], vec![]);
    // Both entries in each batch must see its sources from the first sequence.
    append_values(
        &fx,
        "first",
        vec![assistant("ok", false), assistant("ok", false)],
        manifest(1),
    );
    append_values(
        &fx,
        "second",
        vec![assistant("ok", false), assistant("ok", false)],
        manifest(2),
    );
    let report = grade(
        &fx,
        &suite(vec![contains("text", "AssistantAttempt", "ok")]),
    );
    let instances = &report.runs.0[0].instances.0;
    assert_eq!(instances.len(), 5);
    assert!(matches!(
        instances[0].revision,
        EvalRevisionContext::Mixed { .. }
    ));
    for (instance, revision) in instances[1..].iter().zip([1, 1, 2, 2]) {
        match &instance.revision {
            EvalRevisionContext::Exact {
                context_epoch,
                sources,
            } => {
                assert_eq!(context_epoch.get(), 0);
                assert_eq!(
                    sources
                        .0
                        .iter()
                        .find(|s| s.kind.0 == "eval.skill")
                        .unwrap()
                        .revision
                        .get(),
                    revision
                );
            }
            other => panic!("expected exact: {other:?}"),
        }
    }
}

#[test]
fn host_append_without_session_revision_change_creates_new_report_and_scoped_cursors() {
    let fx = fixture();
    let measurement = suite(vec![contains("text", "AssistantAttempt", "ok")]);
    append_values(&fx, "first", vec![assistant("ok", false)], vec![]);
    let revision = fx.session("session-a").revision;
    let first = grade(&fx, &measurement);
    append_values(&fx, "next", vec![assistant("ok", false)], vec![]);
    assert_eq!(fx.session("session-a").revision, revision);
    let next = grade(&fx, &measurement);
    assert_ne!(first.id, next.id);
    assert_ne!(first.session_sequence.get(), next.session_sequence.get());
    let first_page = page(results(&fx, &fx.user(), "session-a", None));
    assert!(first_page.has_more);
    let cursor = first_page.next_cursor.unwrap().0;
    let second_page = page(results(&fx, &fx.user(), "session-a", Some(&cursor)));
    assert!(!second_page.has_more);
    assert_ne!(first_page.items.0[0].id, second_page.items.0[0].id);
    fx.seed_session("other-session", "agent-a", Some("root-a"));
    assert_eq!(
        fault(results(&fx, &fx.user(), "other-session", Some(&cursor))).code,
        DomainErrorCode::CursorExpired
    );
    let keyed_body = json!({"sessionId":"session-a","suite":measurement});
    let original = fx.command_keyed(
        &fx.user(),
        "evals.grade",
        "scope-bound-grade",
        keyed_body.clone(),
    );
    assert!(!recorded(original).1);
    let replay = fx.command_keyed(
        &fx.user(),
        "evals.grade",
        "scope-bound-grade",
        keyed_body.clone(),
    );
    assert!(recorded(replay).1);
    let confined = fx.ctx("agent-p", PrincipalKind::Agent, Some("project-a"));
    let confined_body = json!({"sessionId":"session-a","suite":measurement});
    applied(fx.command_keyed(
        &confined,
        "evals.grade",
        "confined-replay",
        confined_body.clone(),
    ));
    let prior_revision = fx
        .read(|read| read.authority().principal(&actor("agent-p")))
        .unwrap()
        .revision;
    let by = fx.user();
    fx.write(move |tx| {
        auth::bind_principal(
            tx,
            &by,
            NewPrincipalBinding {
                actor: actor("agent-p"),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: Some(project("project-b")),
                tool_families: None,
                bound_by: actor("user-a"),
            },
            Some(prior_revision),
        )
        .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    });
    // Current stored authority must govern a receipt replay, even when the caller retains
    // its formerly valid invocation context and request key.
    let denied_replay =
        fx.command_keyed(&confined, "evals.grade", "confined-replay", confined_body);
    assert_eq!(rejected(denied_replay), Rejection::PermissionDenied);
    let foreign = fx.ctx("agent-b", PrincipalKind::Agent, Some("project-b"));
    assert_eq!(
        fault(results(&fx, &foreign, "session-a", None)).code,
        DomainErrorCode::NotFound
    );
    assert_eq!(
        refused(fx.command(
            &foreign,
            "evals.grade",
            json!({"sessionId":"session-a","suite":measurement})
        ))
        .code,
        DomainErrorCode::NotFound
    );
}

#[test]
fn recorded_nonbinary_outcomes_stay_out_of_the_pass_rate_and_incomplete_never_passes() {
    let fx = fixture();
    let mut bodies = vec![
        assistant("ok", false),
        assistant("different", false),
        assistant("ok", true),
    ];
    for (ordinal, status) in [
        json!({"kind":"Completed","artifacts":[]}),
        json!({"kind":"Failed","code":"failed"}),
        json!({"kind":"DependencyBlocked","blockerIds":["blocker-a"]}),
        json!({"kind":"CancelledBeforeDispatch"}),
    ]
    .into_iter()
    .enumerate()
    {
        let native = format!("call-{ordinal}");
        bodies.push(json!({"kind":"ToolCallRecorded","toolCall":call(&native,ordinal as u32)}));
        bodies.push(json!({"kind":"ToolResponseRecorded","call":key(&native,ordinal as u32),"outcome":status}));
    }
    // A literal-text check on a valid non-text variant is an explicit grader error.
    bodies.push(json!({"kind":"HostNotice","source":"host-notice","artifact":art()}));
    append_values(&fx, "outcomes", bodies, vec![]);
    let report = grade(
        &fx,
        &suite(vec![
            contains("text", "AssistantAttempt", "ok"),
            tool_check(),
            contains("unsupported", "HostNotice", "ok"),
            contains("absent", "NotARecordedKind", "ok"),
        ]),
    );
    for expected in [
        EvalOutcome::Passed,
        EvalOutcome::Failed,
        EvalOutcome::EvidenceUnavailable,
        EvalOutcome::Unclear,
        EvalOutcome::Cancelled,
        EvalOutcome::GraderError,
        EvalOutcome::NotApplicable,
    ] {
        assert!(outcome(&report, expected));
    }
    assert_eq!(report.coverage.graded.get(), 4);
    assert_eq!(report.coverage.passed.get(), 2);
    assert_eq!(report.coverage.failed.get(), 2);
    assert_eq!(
        serde_json::to_value(report.coverage.pass_rate).unwrap(),
        json!(0.5)
    );
    assert!(!matches!(report.outcome, EvalOutcome::Passed));
    let interrupted = grade(
        &fx,
        &suite(vec![contains("interrupted", "AssistantAttempt", "ok")]),
    );
    assert!(!matches!(interrupted.outcome, EvalOutcome::Passed));
    let incomplete = fixture();
    append_values(
        &incomplete,
        "partial",
        vec![assistant("ok", false), assistant("ok", true)],
        vec![],
    );
    let partial = grade(
        &incomplete,
        &suite(vec![contains("partial", "AssistantAttempt", "ok")]),
    );
    assert_eq!(partial.coverage.passed.get(), 1);
    assert_eq!(partial.coverage.failed.get(), 0);
    assert_eq!(partial.coverage.ungraded.get(), 1);
    assert_eq!(
        serde_json::to_value(partial.coverage.pass_rate).unwrap(),
        json!(1.0)
    );
    assert!(!matches!(partial.outcome, EvalOutcome::Passed));
}

#[test]
fn more_than_one_transcript_page_preserves_every_candidate_in_bounded_coverage() {
    let fx = fixture();
    for chunk in 0..5 {
        let count = if chunk == 4 { 1 } else { 128 };
        append_values(
            &fx,
            &format!("page-{chunk}"),
            (0..count).map(|_| assistant("ok", false)).collect(),
            vec![],
        );
    }
    let report = grade(
        &fx,
        &suite(vec![contains("text", "AssistantAttempt", "ok")]),
    );
    assert_eq!(report.coverage.eligible.get(), 513);
    assert_eq!(
        report.coverage.graded.get() + report.coverage.ungraded.get(),
        513
    );
    assert!(report.runs.0[0].instances.0.len() <= 499);
    assert!(outcome(&report, EvalOutcome::BudgetExhausted));
    assert!(report.coverage.ungraded.get() > 0);
    assert!(!matches!(report.outcome, EvalOutcome::Passed));
    let persisted = page(results(&fx, &fx.user(), "session-a", None));
    assert_eq!(persisted.items.0[0].coverage.eligible.get(), 513);
}

fn record_check(name: &str, source: &str, pointer: &str, expected: Value) -> Value {
    json!({"id":name,"ruleRef":"native recorded source acceptance","measurement":"Proxy",
        "predicate":{"kind":"RecordFieldEquals","source":source,"pointer":pointer,"expected":expected}})
}

#[test]
fn every_native_record_source_is_graded_without_replaying_or_mutating_observed_work() {
    let fx = fixture();
    fx.store.with_writer_connection(|c| {
        super::super::super::conn::db(c.execute_batch("INSERT INTO workflow_statuses VALUES('ready-a','project-a','Ready','Ready',1,1); UPDATE work_items SET status_id='ready-a' WHERE id='root-a';"))
    }).unwrap();
    let user = fx.user();
    fx.write(move |tx| {
        for kind in [LinkKind::Git, LinkKind::PullRequest, LinkKind::Review] {
            let is_review = kind == LinkKind::Review;
            record_link(
                tx,
                &user,
                NewHistoryLink {
                    root_id: root("root-a"),
                    kind,
                    git: (!is_review).then(|| GitFacts {
                        repository_id: id("repo-a"),
                        worktree_id: None,
                        branch: Some("feature/recorded".into()),
                        pull_request_url: Some("https://example.test/pr/42".into()),
                        head: serde_json::from_value(json!({"format":"Sha1","hex":"d".repeat(40)}))
                            .unwrap(),
                    }),
                    artifact: None,
                    target: is_review.then(|| ("review_run".into(), "review-a".into())),
                    summary: "Native recorded acceptance".into(),
                    supersedes: None,
                },
            )?
            .map_err(|r| StorageError::InvalidRecord(format!("{r:?}")))?;
        }
        let event_id =
            devworks_core::protocol::EventId(tx.new_id(devworks_core::ports::IdKind::Event));
        tx.history().append(NewEvent {
            id: event_id,
            actor: actor("user-a"),
            project_id: Some(project("project-a")),
            root_id: Some(root("root-a")),
            operation_id: None,
            aggregate_kind: "custom_native_fact".into(),
            aggregate_id: "acceptance-a".into(),
            aggregate_revision: 1,
            kind: "NativeAcceptanceRecorded".into(),
            payload: json!({"accepted":true}),
        })?;
        Ok(())
    });
    let before = protected_rows(&fx);
    let measurement = suite(vec![
        record_check("session", "Session", "/title", json!("Seeded")),
        record_check("history", "HistoryEvent", "/rootId", json!("root-a")),
        record_check("git", "GitLink", "/body/head/hex", json!("d".repeat(40))),
        record_check(
            "pr",
            "PullRequestLink",
            "/body/pullRequestUrl",
            json!("https://example.test/pr/42"),
        ),
        record_check("review", "ReviewLink", "/targetId", json!("review-a")),
        record_check("work", "WorkState", "/statusId", json!("ready-a")),
    ]);
    let report = grade(&fx, &measurement);
    assert!(matches!(report.outcome, EvalOutcome::Passed));
    for kind in [
        "Session",
        "HistoryEvent",
        "GitLink",
        "PullRequestLink",
        "ReviewLink",
        "WorkState",
    ] {
        assert!(
            report.runs.0[0]
                .instances
                .0
                .iter()
                .any(|i| i.source.as_ref().is_some_and(|s| s.kind.0 == kind)),
            "missing {kind}"
        );
    }
    assert!(
        report.runs.0[0]
            .instances
            .0
            .iter()
            .all(|i| matches!(i.revision, EvalRevisionContext::Mixed { .. }))
    );
    assert_eq!(protected_rows(&fx), before);
    // An arbitrary native domain event outside the public command-history projection must
    // still be visible to the full durable HistoryEvent source.
    let custom = grade(
        &fx,
        &suite(vec![record_check(
            "custom",
            "HistoryEvent",
            "/kind",
            json!("NativeAcceptanceRecorded"),
        )]),
    );
    assert!(
        custom.runs.0[0]
            .instances
            .0
            .iter()
            .any(|i| matches!(i.outcome, EvalOutcome::Passed))
    );
    let failures = grade(
        &fx,
        &suite(vec![
            record_check("wrong", "Session", "/title", json!("different")),
            record_check("missing", "Session", "/does-not-exist", Value::Null),
            record_check("invalid", "Session", "/title~2", Value::Null),
        ]),
    );
    for expected in [
        EvalOutcome::Failed,
        EvalOutcome::EvidenceUnavailable,
        EvalOutcome::GraderError,
    ] {
        assert!(outcome(&failures, expected));
    }
    assert_eq!(protected_rows(&fx), before);
    let transcript = fx.session("session-a");
    fx.write(|tx| {
        let mut work = tx.work().work(&id("root-a"))?.unwrap();
        let prior = work.revision;
        work.status_id = None;
        work.revision += 1;
        tx.work().update_work(&work, prior)
    });
    let changed = grade(&fx, &measurement);
    assert_ne!(
        changed.id, report.id,
        "a changed recorded source must produce a fresh report"
    );
    assert!(matches!(changed.outcome, EvalOutcome::Failed));
    assert_ne!(
        changed.evidence_digest.as_str(),
        report.evidence_digest.as_str()
    );
    assert_eq!(fx.session("session-a"), transcript);
    let old_id = report.id.clone();
    let mut reopened = fx.restart();
    enable(&mut reopened);
    let stored = reopened
        .read(|snapshot| snapshot.evals().eval_run(&old_id))
        .unwrap()
        .report;
    let work = stored
        .pointer("/runs/0/instances")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i.get("checkId") == Some(&json!("work")))
        .unwrap();
    assert_eq!(
        work.pointer("/observedValue/value"),
        Some(&json!("ready-a")),
        "persisted report must retain the earlier observed mutable status after change and reopen"
    );
}

#[test]
fn native_history_grading_excludes_its_own_root_scoped_audit_receipts() {
    let fx = fixture();
    let worker = fx.worker("agent-a", "session-a");
    let missing = grade(
        &fx,
        &suite(vec![record_check(
            "missing-git",
            "GitLink",
            "/revision",
            json!("missing"),
        )]),
    );
    assert!(outcome(&missing, EvalOutcome::EvidenceUnavailable));
    // Provide at least one ordinary domain event, then grade with root-bound authority so the
    // gateway's own evals.grade CommandDecided event is attached to the same root.
    append_values(
        &fx,
        "history-input",
        vec![assistant("original", false)],
        vec![],
    );
    let measurement = suite(vec![record_check(
        "history",
        "HistoryEvent",
        "/rootId",
        json!("root-a"),
    )]);
    let body = json!({"sessionId":"session-a","suite":measurement});
    let extract = |reply| match applied(reply) {
        OperationResult::EvalsGrade { value } => value,
        other => panic!("{other:?}"),
    };
    let first = extract(fx.command(&worker, "evals.grade", body.clone()));
    let attached_eval_audit = fx.read(|snapshot| {
        Ok(snapshot
            .history()
            .page(&EventFilter::Root(root("root-a")), 0, 500)?
            .events
            .iter()
            .any(|event| event.payload.get("operation") == Some(&json!("evals.grade"))))
    });
    assert!(
        attached_eval_audit,
        "feedback regression requires an eval audit event on the observed root"
    );
    let second = extract(fx.command(&worker, "evals.grade", body));
    assert_eq!(
        first.id, second.id,
        "evaluation audit output must not create a new source digest"
    );
    assert_eq!(
        first.coverage.eligible.get(),
        second.coverage.eligible.get()
    );
}
