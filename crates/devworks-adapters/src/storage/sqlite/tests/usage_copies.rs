//! Migration 54: session and job usage items live only in their usage origins. A store seeded
//! the old way (source rows in session_usage and job_run_usage, some already ingested and some
//! not) upgrades to the same origins the ingestion code writes, and the session and job usage
//! reads return what the dropped tables returned.
use super::{Harness, T0, through};
use crate::storage::sqlite::jobs::pico;
use crate::storage::sqlite::{MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{EventId, Id, OperationId, ProjectId, RootId};
use devworks_core::services::usage::ingest;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}

/// (session, batch, ordinal, amountUsd, priceKind, priceOrigin, incomplete).
type SessionRow = (
    &'static str,
    &'static str,
    i64,
    Option<&'static str>,
    &'static str,
    &'static str,
    bool,
);
/// Batch `b2`'s item and job item 3 were already ingested when the old build wrote them; the
/// rest predate migration 28 and were never bootstrapped.
const SESSION_ROWS: [SessionRow; 5] = [
    (
        "s1",
        "b1",
        1,
        Some("0.0000015"),
        "Reported",
        "ProviderReported",
        false,
    ),
    (
        "s1",
        "b1",
        2,
        Some("12.000000499999"),
        "Estimated",
        "UserConfigured",
        true,
    ),
    (
        "s1",
        "b2",
        1,
        Some("0.0000005"),
        "Estimated",
        "OfficialSnapshot",
        false,
    ),
    ("s1", "b4", 1, None, "Untracked", "Untracked", false),
    ("s2", "b3", 1, Some("0"), "Free", "LocalFree", false),
];
/// (seq, amount_usd, price_kind, price_origin, incomplete).
const JOB_ROWS: [(i64, Option<&str>, &str, &str, bool); 4] = [
    (
        1,
        Some("0.0000014999"),
        "Reported",
        "ProviderReported",
        false,
    ),
    (2, None, "Untracked", "Unknown", true),
    (
        3,
        Some("2.5"),
        "Estimated",
        "SubscriptionApiEquivalent",
        false,
    ),
    (
        4,
        Some("0.000000500001"),
        "Estimated",
        "OfficialSnapshot",
        true,
    ),
];
const INGESTED_SESSION: (&str, &str, i64) = ("s1", "b2", 1);
const INGESTED_JOB: i64 = 3;

fn item(row: &SessionRow) -> Value {
    let (session, batch, ordinal, amount, kind, origin, incomplete) = *row;
    let mut v = json!({"requestId": format!("request-{session}-{batch}-{ordinal}"),
        "purpose": "Conversation", "modelId": "sol", "inputTokens": "10", "outputTokens": "5",
        "cacheReadTokens": "4", "cacheWriteTokens": "0", "priceKind": kind,
        "priceOrigin": origin, "incomplete": incomplete});
    if let Some(a) = amount {
        v["amountUsd"] = json!(a);
    }
    v
}
fn job_usage(row: &(i64, Option<&str>, &str, &str, bool)) -> StoredJobUsage {
    let (seq, amount, kind, origin, incomplete) = *row;
    StoredJobUsage {
        run_id: id("run"),
        seq,
        request_id: id(&format!("job-request-{seq}")),
        purpose: "Conversation".into(),
        model_id: id("sol"),
        input_tokens: seq * 100,
        output_tokens: seq,
        cache_read_tokens: 3,
        cache_write_tokens: 1,
        amount_usd: amount.map(str::to_owned),
        price_kind: kind.into(),
        price_origin: origin.into(),
        price_revision: (seq == 3).then_some(7),
        incomplete,
        recorded_at_ms: T0 + seq,
    }
}

/// Sessions `s1` (project, feature root) and `s2` (workspace), batches b1..b4 and one job run,
/// written through repositories none of which touches a table migration 54 changes. Returns
/// each batch's committed sequence.
fn seed(s: &SqliteStore) -> Vec<(String, i64)> {
    let actor = s.info().host_actor;
    run_write(s, WriteOptions::default(), move |tx| {
        tx.projects().insert_project(&StoredProject {
            id: ProjectId(id("project")),
            name: "Project".into(),
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
        tx.work().insert_work(&StoredWork {
            id: id("feature"),
            project_id: ProjectId(id("project")),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: None,
            title: "feature".into(),
            priority: 0,
            manual_order: 0,
            revision: 1,
            scope_revision: 1,
            created_at_ms: T0,
            updated_at_ms: T0,
            created_by: None,
            updated_by: None,
            search_key: "feature".into(),
            caller_ref: None,
            completion: None,
            brief: json!({}),
        })?;
        for (name, project, root) in [
            (
                "s1",
                Some(ProjectId(id("project"))),
                Some(RootId(id("feature"))),
            ),
            ("s2", None, None),
        ] {
            tx.sessions().insert_session(&StoredSession {
                id: id(name),
                conversation_id: id(&format!("conversation-{name}")),
                agent_id: actor.clone(),
                project_id: project,
                root_id: root,
                title: name.into(),
                context_epoch: 0,
                revision: 1,
                created_by: actor.clone(),
                created_at_ms: T0,
                closed_at_ms: None,
                closed_by: None,
                next_sequence: 1,
                checkpoint_sequence: 0,
            })?;
        }
        let mut batches = Vec::new();
        for (n, (session, batch)) in [("s1", "b1"), ("s1", "b2"), ("s2", "b3"), ("s1", "b4")]
            .into_iter()
            .enumerate()
        {
            let session = tx.sessions().session(&id(session))?.unwrap();
            let event = id(&format!("event-{batch}"));
            let sequence = tx.history().append(NewEvent {
                id: EventId(event.clone()),
                actor: actor.clone(),
                project_id: session.project_id.clone(),
                root_id: session.root_id.clone(),
                operation_id: None,
                aggregate_kind: "session".into(),
                aggregate_id: batch.into(),
                aggregate_revision: 1,
                kind: "UsageRecorded".into(),
                payload: json!({}),
            })?;
            tx.sessions().insert_batch(&StoredBatch {
                batch_id: id(batch),
                session_id: session.id.clone(),
                payload_digest: "a".repeat(64),
                actor: actor.clone(),
                context_epoch: 0,
                first_sequence: n as i64 + 1,
                last_sequence: n as i64 + 1,
                event_id: event,
                committed_sequence: sequence,
                accepted_at_ms: T0 + 10 * n as i64,
            })?;
            batches.push((batch.to_owned(), sequence));
        }
        tx.jobs().insert_job(&StoredJob {
            id: id("job"),
            revision: 1,
            project_id: Some(ProjectId(id("project"))),
            title: "job".into(),
            kind: "Inference".into(),
            schedule: "{\"kind\":\"Manual\"}".into(),
            timezone: "UTC".into(),
            definition: "{}".into(),
            enabled: false,
            authorized: false,
            authorized_by: None,
            hold: None,
            activated_epoch: 0,
            project_epoch: 0,
            anchor_at_ms: T0,
            next_at_ms: None,
            interrupted_run_id: None,
            created_at_ms: T0,
            updated_at_ms: T0,
            deleted_at_ms: None,
        })?;
        let operation = OperationId(id("operation-job"));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "jobs.run".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        tx.jobs().insert_run(&StoredJobRun {
            id: id("run"),
            job_id: id("job"),
            seq: 1,
            project_id: Some(ProjectId(id("project"))),
            job_revision: 1,
            definition: "{}".into(),
            project_epoch: 0,
            restore_epoch: 0,
            occurrence_key: "job".into(),
            occurrence_at_ms: T0,
            trigger: "Manual".into(),
            status: "Succeeded".into(),
            revision: 1,
            operation_id: Some(operation),
            inner_operation_id: None,
            boot_id: None,
            started_at_ms: Some(T0),
            finished_at_ms: Some(T0),
            reason: None,
            hold: None,
            cancel_requested: false,
            resumptions: 0,
            session_id: None,
            agent_actor: None,
            skipped_for: None,
            checkpoint: None,
            created_at_ms: T0,
        })?;
        Ok(batches)
    })
    .unwrap()
}

fn stored_usage(row: &SessionRow, batches: &[(String, i64)]) -> StoredUsage {
    let (session, batch, ordinal, ..) = *row;
    let s1 = session == "s1";
    StoredUsage {
        session_id: id(session),
        batch_id: id(batch),
        ordinal,
        project_id: s1.then(|| ProjectId(id("project"))),
        root_id: s1.then(|| RootId(id("feature"))),
        committed_sequence: batches.iter().find(|(b, _)| b == batch).unwrap().1,
        item: item(row),
    }
}

fn origins(s: &SqliteStore) -> Vec<StoredUsageOrigin> {
    read_snapshot(s, &ReadOptions::default(), |r| {
        r.usage()
            .usage_origins(&UsageOriginFilter::default(), None, 500)
    })
    .unwrap()
}

/// The pre-migration-54 read of a root's session usage (the `session_usage` statement).
fn old_session_usage(c: &Connection, root: Option<&str>) -> Vec<(SequenceKey, StoredUsage)> {
    let mut st = c
        .prepare(
            "SELECT batch_id,ordinal,session_id,project_id,root_id,committed_seq,item_json
             FROM session_usage WHERE root_id IS ?1 ORDER BY committed_seq,ordinal",
        )
        .unwrap();
    st.query_map(params![root], |r| {
        let item: String = r.get(6)?;
        let ordinal: i64 = r.get(1)?;
        let sequence: i64 = r.get(5)?;
        Ok((
            SequenceKey {
                sequence,
                source: 3,
                id: format!("{ordinal:06}"),
            },
            StoredUsage {
                batch_id: id(&r.get::<_, String>(0)?),
                ordinal,
                session_id: id(&r.get::<_, String>(2)?),
                project_id: r.get::<_, Option<String>>(3)?.map(|p| ProjectId(id(&p))),
                root_id: r.get::<_, Option<String>>(4)?.map(|p| RootId(id(&p))),
                committed_sequence: sequence,
                item: serde_json::from_str(&item).unwrap(),
            },
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The pre-migration-54 run usage page and totals (the `job_run_usage` statements).
fn old_job_usage(c: &Connection) -> (Vec<StoredJobUsage>, JobUsageTotals) {
    let mut st = c
        .prepare(
            "SELECT run_id,seq,request_id,purpose,model_id,input_tokens,output_tokens,
             cache_read_tokens,cache_write_tokens,amount_usd,price_kind,price_origin,price_revision,
             incomplete,recorded_at FROM job_run_usage WHERE run_id='run' ORDER BY seq",
        )
        .unwrap();
    let rows: Vec<StoredJobUsage> = st
        .query_map([], |r| {
            Ok(StoredJobUsage {
                run_id: id(&r.get::<_, String>(0)?),
                seq: r.get(1)?,
                request_id: id(&r.get::<_, String>(2)?),
                purpose: r.get(3)?,
                model_id: id(&r.get::<_, String>(4)?),
                input_tokens: r.get(5)?,
                output_tokens: r.get(6)?,
                cache_read_tokens: r.get(7)?,
                cache_write_tokens: r.get(8)?,
                amount_usd: r.get(9)?,
                price_kind: r.get(10)?,
                price_origin: r.get(11)?,
                price_revision: r.get(12)?,
                incomplete: r.get(13)?,
                recorded_at_ms: r.get(14)?,
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let (items, untracked, incomplete): (i64, i64, i64) = c
        .query_row(
            "SELECT count(*), ifnull(sum(price_kind='Untracked'),0), ifnull(sum(incomplete),0)
             FROM job_run_usage WHERE run_id='run'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let mut amounts = c
        .prepare(
            "SELECT amount_usd FROM job_run_usage WHERE run_id='run' AND amount_usd IS NOT NULL",
        )
        .unwrap();
    let tracked = amounts
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|a| pico(&a.unwrap()).unwrap())
        .sum();
    (
        rows,
        JobUsageTotals {
            items,
            tracked_pico_usd: tracked,
            untracked,
            incomplete,
        },
    )
}

#[test]
fn upgrade_moves_session_and_job_usage_into_origins_with_the_same_reads_and_totals() {
    // The origins the ingestion code writes for every item, in a store at this build's schema.
    let reference = Harness::new();
    let r = SqliteStore::open_with(reference.config(), MIGRATIONS).unwrap();
    let batches = seed(&r);
    let rows: Vec<_> = SESSION_ROWS
        .iter()
        .map(|row| stored_usage(row, &batches))
        .collect();
    let ingested = rows.clone();
    run_write(&r, WriteOptions::default(), move |tx| {
        for u in &ingested {
            ingest::session_usage(tx, u)?;
        }
        for row in &JOB_ROWS {
            ingest::job_usage(tx, &job_usage(row))?;
        }
        Ok(())
    })
    .unwrap();
    let expected = origins(&r);
    assert_eq!(expected.len(), SESSION_ROWS.len() + JOB_ROWS.len());

    // The same store as the old build left it: every item in its source table, and only the
    // items written after migration 28 also in usage_origins.
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(53)).unwrap();
    assert_eq!(seed(&old), batches);
    drop(old);
    let c = h.raw();
    for u in &rows {
        c.execute(
            "INSERT INTO session_usage(batch_id,ordinal,session_id,project_id,root_id,committed_seq,item_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                u.batch_id.as_str(),
                u.ordinal,
                u.session_id.as_str(),
                u.project_id.as_ref().map(|p| p.0.as_str()),
                u.root_id.as_ref().map(|r| r.0.as_str()),
                u.committed_sequence,
                u.item.to_string()
            ],
        )
        .unwrap();
    }
    for row in &JOB_ROWS {
        let u = job_usage(row);
        c.execute(
            "INSERT INTO job_run_usage(run_id,seq,request_id,purpose,model_id,input_tokens,output_tokens,
             cache_read_tokens,cache_write_tokens,amount_usd,price_kind,price_origin,price_revision,
             incomplete,recorded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                u.run_id.as_str(),
                u.seq,
                u.request_id.as_str(),
                u.purpose,
                u.model_id.as_str(),
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.amount_usd,
                u.price_kind,
                u.price_origin,
                u.price_revision,
                u.incomplete,
                u.recorded_at_ms
            ],
        )
        .unwrap();
    }
    let ingested_session =
        json!([INGESTED_SESSION.0, INGESTED_SESSION.1, INGESTED_SESSION.2]).to_string();
    let ingested_job = json!(["run", INGESTED_JOB]).to_string();
    for o in expected.iter().filter(|o| {
        (o.source_kind == "SessionUsage" && o.source_id == ingested_session)
            || (o.source_kind == "JobUsage" && o.source_id == ingested_job)
    }) {
        c.execute(
            "INSERT INTO usage_origins(source_kind,source_id,attempt_id,request_id,project_id,feature_id,
             work_id,session_id,job_run_id,payload_json,recorded_at,priced_micro_usd,classification,
             monetary_complete,incomplete,unknown_tail) VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0,?13,?14)",
            params![
                o.source_kind,
                o.source_id,
                o.request_id.as_ref().map(Id::as_str),
                o.project_id.as_ref().map(|p| p.0.as_str()),
                o.feature_id.as_ref().map(Id::as_str),
                o.work_id.as_ref().map(Id::as_str),
                o.session_id.as_ref().map(Id::as_str),
                o.job_run_id.as_ref().map(Id::as_str),
                o.payload.to_string(),
                o.recorded_at_ms,
                o.priced_micro_usd,
                o.classification,
                o.incomplete,
                o.unknown_tail
            ],
        )
        .unwrap();
    }
    let old_root = old_session_usage(&c, Some("feature"));
    let old_workspace = old_session_usage(&c, None);
    let (old_runs, old_totals) = old_job_usage(&c);
    assert_eq!(old_root.len(), 4);
    assert_eq!(old_runs.len(), JOB_ROWS.len());
    drop(c);

    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    let c = h.raw();
    for table in ["session_usage", "job_run_usage"] {
        let exists: bool = c
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists, "{table} survived the upgrade");
    }
    // Rows without an origin were copied exactly as ingestion writes them; rows with one were
    // discarded, leaving that origin (its committed sequence filled in) and no duplicate.
    assert_eq!(origins(&s), expected);

    read_snapshot(&s, &ReadOptions::default(), |r| {
        let root = r.sessions().usage(&RootId(id("feature")), None, 500)?;
        assert_eq!(root, old_root);
        let paged = r
            .sessions()
            .usage(&RootId(id("feature")), Some(&root[1].0), 500)?;
        assert_eq!(paged, old_root[2..]);
        let inference = |facts: Vec<UsageFact>| -> Vec<(SequenceKey, StoredUsage)> {
            facts
                .into_iter()
                .filter_map(|f| match f {
                    UsageFact::Inference { key, usage } => Some((key, usage)),
                    UsageFact::Tool { .. } => None,
                })
                .collect()
        };
        assert_eq!(
            inference(r.sessions().usage_facts(
                &HistoryScope::Root(RootId(id("feature"))),
                None,
                500
            )?),
            old_root
        );
        assert_eq!(
            inference(r.sessions().usage_facts(
                &HistoryScope::Project(ProjectId(id("project"))),
                None,
                500
            )?),
            old_root
        );
        assert_eq!(
            inference(
                r.sessions()
                    .usage_facts(&HistoryScope::Workspace, None, 500)?
            ),
            old_workspace
        );
        assert_eq!(r.jobs().run_usage(&id("run"), None, 500)?, old_runs);
        assert_eq!(r.jobs().run_usage(&id("run"), Some(2), 1)?, old_runs[2..3]);
        assert_eq!(r.jobs().usage_totals(&id("run"))?, old_totals);
        Ok(())
    })
    .unwrap();
    // Each read seeks its own index rather than scanning every origin of the kind.
    for (filter, index) in [
        (
            "source_kind='SessionUsage' AND work_id IS ?1 AND committed_seq>=?2 ORDER BY committed_seq",
            "usage_origins_session_root",
        ),
        (
            "source_kind='SessionUsage' AND project_id IS ?1 AND committed_seq>=?2 ORDER BY committed_seq",
            "usage_origins_session_project",
        ),
        (
            "source_kind='JobUsage' AND job_run_id=?1 AND ?2 IS NOT NULL",
            "usage_origins_job_request",
        ),
    ] {
        let detail: String = c
            .query_row(
                &format!("EXPLAIN QUERY PLAN SELECT * FROM usage_origins WHERE {filter} LIMIT 1"),
                params!["x", 0],
                |r| r.get(3),
            )
            .unwrap();
        assert!(detail.contains(index), "{filter}: {detail}");
    }
}

#[test]
fn job_usage_is_recorded_once_per_request_with_the_next_sequence() {
    let h = Harness::new();
    let s = SqliteStore::open_with(h.config(), MIGRATIONS).unwrap();
    seed(&s);
    let first = job_usage(&JOB_ROWS[0]);
    run_write(&s, WriteOptions::default(), move |tx| {
        let mut u = first.clone();
        u.seq = 0;
        let prepared = tx.jobs().prepare_usage(&u)?;
        assert_eq!(prepared.seq, 1);
        ingest::job_usage(tx, &prepared)?;
        assert!(matches!(
            tx.jobs().prepare_usage(&u),
            Err(StorageError::Duplicate { .. })
        ));
        let mut next = job_usage(&JOB_ROWS[1]);
        next.seq = 0;
        assert_eq!(tx.jobs().prepare_usage(&next)?.seq, 2);
        next.purpose = String::new();
        assert!(tx.jobs().prepare_usage(&next).is_err());
        Ok(())
    })
    .unwrap();
}
