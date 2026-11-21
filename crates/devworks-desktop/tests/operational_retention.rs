//! Native gateway, Jobs, Settings and SQLite qualification for operational retention.
mod common;
use common::*;
use devworks_core::ports::storage::read_snapshot;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{
    JobDefinition, PruneInspection, PrunePhase, PrunePreview, Schedule,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::registry::SchemaRevision;
use devworks_core::services::jobs::{argument_schema, capability_id};
use devworks_desktop::{backend::Backend, frame::fixture, ops};
use eframe::egui::accesskit::Role;
use rusqlite::Connection;
use serde_json::json;
use std::time::{Duration, Instant};

const DIAGNOSTICS: &str = "maintenance.prune_diagnostics";
const INDEX_RUNS: &str = "maintenance.prune_index_runs";
const MESSAGES: &str = "maintenance.prune_messages";
const SESSIONS: &str = "maintenance.prune_sessions";
const SCHEDULE: &str = "maintenance.scheduled_prune";
const OLD: i64 = 1_700_000_000_000;
const CUTOFF: i64 = OLD + 50;

fn database(root: &Root) -> Connection {
    let database = Connection::open(root.0.join("workspace.sqlite")).unwrap();
    database.busy_timeout(Duration::from_secs(5)).unwrap();
    database.pragma_update(None, "foreign_keys", true).unwrap();
    database
}

fn diagnostics(conn: &Connection) {
    for (id, origin, time) in [
        ("diag-old", "Host", OLD),
        ("diag-human", "Reported", OLD),
        ("diag-equal", "Host", CUTOFF),
    ] {
        conn.execute(
            "INSERT INTO diagnostic_records(id,recorded_at,severity,severity_rank,component,code,message,origin,actor_id)
             VALUES(?1,?2,'Info',0,'retention.test','test','fixture',?3,'retention-machine')",
            rusqlite::params![id,time,origin],
        ).unwrap();
    }
}

fn seed(conn: &Connection) {
    conn.execute_batch("
        INSERT INTO actors VALUES
            ('retention-machine','agent','Test automation'),
            ('retention-human','human','Test author');
        INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at)
            VALUES('retention-project','Fixture project','Active',1,1,1700000000000);
        INSERT INTO conversations(id,created_at) VALUES
            ('conv-eligible',1700000000000),('conv-authored',1700000000000),
            ('conv-inbox',1700000000000),('conv-live',1700000000000);
        INSERT INTO sessions(id,conversation_id,agent_id,context_epoch,revision,created_by,created_at,closed_at,closed_by,title) VALUES
            ('session-eligible','conv-eligible','retention-machine',0,1,'retention-machine',1700000000000,1700000000001,'retention-machine','Operational label'),
            ('session-authored','conv-authored','retention-machine',0,1,'retention-human',1700000000000,1700000000001,'retention-machine','Authored chat'),
            ('session-inbox','conv-inbox','retention-machine',0,1,'retention-machine',1700000000000,1700000000001,'retention-machine','Recipient'),
            ('session-live','conv-live','retention-machine',0,1,'retention-machine',1700000000000,NULL,NULL,'Active');
        INSERT INTO agent_messages(id,project_id,channel,message_class,body,attachments_json,blocking,revision,created_at,from_actor_id,target_session_id) VALUES
            ('message-eligible','retention-project','Session','Info','Operational notice','[]',0,1,1700000000000,'retention-machine','session-inbox'),
            ('message-authored','retention-project','Session','Info','Human history','[]',0,1,1700000000000,'retention-human','session-inbox'),
            ('message-pending','retention-project','Session','Info','Unread notice','[]',0,1,1700000000000,'retention-machine','session-inbox'),
            ('message-head','retention-project','Session','Info','Replay boundary','[]',0,1,1700000000000,'retention-machine','session-inbox');
        INSERT INTO agent_message_deliveries(sequence,message_id,project_id,recipient_session_id,recipient_human,state,revision,delivered_at,read_at) VALUES
            (1,'message-eligible','retention-project','session-inbox',0,'Read',2,1700000000001,1700000000002),
            (2,'message-authored','retention-project','session-inbox',0,'Read',2,1700000000001,1700000000002),
            (3,'message-pending','retention-project','session-inbox',0,'Delivered',1,1700000000001,NULL),
            (4,'message-head','retention-project','session-inbox',0,'Read',2,1700000000001,1700000000002);
        UPDATE agent_message_sequence_heads SET head=4 WHERE kind='delivery';
        INSERT INTO repositories(id,canonical_location,revision) VALUES('retention-repo','fixture://retention',1);
        INSERT INTO index_runs(id,batch_id,kind,repository_id,state,stage,revision,attempt,max_attempts,
            config_revision,config_digest,completed_units,deterministic,warnings_json,requested_by,created_at,finished_at,next_attempt_at)
        VALUES
            ('run-old','batch-old','Index','retention-repo','Cancelled','Stopped',1,0,1,
              1,printf('%064d',0),0,1,'[]','retention-machine',1700000000000,1700000000001,0),
            ('run-failed','batch-failed','Index','retention-repo','Failed','Failed',1,0,1,
              1,printf('%064d',0),0,1,'[]','retention-machine',1700000000002,1700000000003,0),
            ('run-current','batch-current','Index','retention-repo','Succeeded','Completed',1,0,1,
              1,printf('%064d',0),0,1,'[]','retention-machine',1700000000004,1700000000005,0);
    ").unwrap();
    diagnostics(conn);
}

fn preview(b: &dyn Backend, op: &str, limit: u32) -> PrunePreview {
    let frame = fixture::command(
        b,
        op,
        json!({
            "previewOnly":true,"olderThanMs":CUTOFF.to_string(),"maxItems":limit,
        }),
    );
    match ops::command_result(b.command(op, &frame)) {
        Ok(
            OperationResult::MaintenancePruneDiagnostics { value }
            | OperationResult::MaintenancePruneIndexRuns { value }
            | OperationResult::MaintenancePruneMessages { value }
            | OperationResult::MaintenancePruneSessions { value },
        ) => value,
        result => panic!("{result:?}"),
    }
}

fn confirm_frame(b: &dyn Backend, op: &str, preview: &PrunePreview, limit: u32) -> Vec<u8> {
    fixture::command(
        b,
        op,
        json!({
            "previewOnly":false,"previewId":preview.id,"expectedRevision":preview.revision,
            "olderThanMs":CUTOFF.to_string(),"maxItems":limit,
        }),
    )
}

fn inspect(b: &dyn Backend, id: &Id) -> PruneInspection {
    let op = "maintenance.inspect";
    let frame = fixture::query(b, op, json!({"previewId":id}));
    match ops::query_result(b.query(op, &frame)) {
        Ok((OperationResult::MaintenanceInspect { value }, _)) => value,
        result => panic!("{result:?}"),
    }
}

fn settled(b: &dyn Backend, id: &Id) -> PruneInspection {
    let until = Instant::now() + SETTLE_BOUND;
    loop {
        let saved = inspect(b, id);
        if saved.outcome.is_some() {
            return saved;
        }
        assert!(Instant::now() < until, "{saved:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn exists(conn: &Connection, table: &str, id: &str) -> bool {
    conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),
        [id],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn four_selectors_apply_the_reviewed_rows_and_preserve_holds() {
    let _case = native_runtime_case();
    let root = Root::new("operational-retention-exact");
    let app = start(&root);
    let conn = database(&root);
    seed(&conn);
    let b = app.backend();
    for (op, table, eligible, held) in [
        (
            DIAGNOSTICS,
            "diagnostic_records",
            "diag-old",
            &["diag-human", "diag-equal"][..],
        ),
        (
            INDEX_RUNS,
            "index_runs",
            "run-old",
            &["run-failed", "run-current"][..],
        ),
        (
            MESSAGES,
            "agent_messages",
            "message-eligible",
            &["message-authored", "message-pending", "message-head"][..],
        ),
        (
            SESSIONS,
            "sessions",
            "session-eligible",
            &["session-authored", "session-inbox", "session-live"][..],
        ),
    ] {
        let saved = preview(&*b, op, 20);
        let candidate_ids = saved
            .candidate_ids
            .0
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(candidate_ids, [eligible], "{op}");
        let detail = inspect(&*b, &saved.id);
        for id in held {
            assert!(
                detail.held.0.iter().any(|row| row.id.as_str() == *id),
                "{id}: {detail:?}"
            );
        }
        let frame = confirm_frame(&*b, op, &saved, 20);
        let effect = ops::accepted_decision(b.command(op, &frame)).unwrap();
        let applied = settled(&*b, &saved.id);
        assert!(matches!(applied.phase, PrunePhase::Applied), "{applied:?}");
        assert_eq!(applied.outcome.unwrap().records_removed.get(), 1);
        assert_eq!(applied.operation_id.as_ref(), Some(&effect));
        assert_eq!(
            ops::accepted_decision(b.command(op, &frame)).unwrap(),
            effect
        );
        assert!(!exists(&conn, table, eligible));
        for id in held {
            assert!(exists(&conn, table, id), "{id}");
        }
    }
    assert!(!exists(&conn, "conversations", "conv-eligible"));
    let head: i64 = conn
        .query_row(
            "SELECT head FROM agent_message_sequence_heads WHERE kind='delivery'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(head, 4);
    let deliveries: i64 = conn
        .query_row("SELECT count(*) FROM agent_message_deliveries", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(deliveries, 3);
}

#[test]
fn confirmation_refuses_a_changed_candidate_and_never_substitutes_a_new_one() {
    let _case = native_runtime_case();
    let root = Root::new("operational-retention-drift");
    let app = start(&root);
    let conn = database(&root);
    diagnostics(&conn);
    let b = app.backend();
    let saved = preview(&*b, DIAGNOSTICS, 1);
    assert!(
        ops::accepted_decision(b.command(INDEX_RUNS, &confirm_frame(&*b, INDEX_RUNS, &saved, 1)))
            .is_err()
    );
    assert!(
        ops::accepted_decision(b.command(DIAGNOSTICS, &confirm_frame(&*b, DIAGNOSTICS, &saved, 2)))
            .is_err()
    );
    conn.execute(
        "UPDATE diagnostic_records SET evidence_json='{}' WHERE id='diag-old'",
        [],
    )
    .unwrap();
    let late_row = "INSERT INTO diagnostic_records(id,recorded_at,severity,severity_rank,component,code,message,origin,actor_id)
         VALUES(?1,?2,'Info',0,'retention.test','test',' fixture','Host','retention-machine')";
    conn.execute(late_row, ("diag-added", OLD + 10)).unwrap();
    assert!(
        ops::accepted_decision(b.command(DIAGNOSTICS, &confirm_frame(&*b, DIAGNOSTICS, &saved, 1)))
            .is_err()
    );
    assert!(exists(&conn, "diagnostic_records", "diag-old"));
    assert!(exists(&conn, "diagnostic_records", "diag-added"));
    let fresh = preview(&*b, DIAGNOSTICS, 20);
    conn.execute(late_row, ("diag-newly-eligible", OLD + 11))
        .unwrap();
    let frame = confirm_frame(&*b, DIAGNOSTICS, &fresh, 20);
    let operation = ops::accepted_decision(b.command(DIAGNOSTICS, &frame)).unwrap();
    assert!(matches!(settled(&*b, &fresh.id).phase, PrunePhase::Applied));
    assert!(exists(&conn, "diagnostic_records", "diag-newly-eligible"));
    drop(conn);
    drop(b);
    app.shutdown().expect("first native boot stops");
    let restarted = start(&root);
    let b = restarted.backend();
    assert_eq!(
        ops::accepted_decision(b.command(DIAGNOSTICS, &frame)).unwrap(),
        operation
    );
    assert_eq!(
        inspect(&*b, &fresh.id)
            .outcome
            .unwrap()
            .records_removed
            .get(),
        1
    );
}

#[test]
fn an_explicit_native_job_firing_applies_one_bounded_preview() {
    let _case = native_runtime_case();
    let root = Root::new("operational-retention-scheduled");
    let app = start(&root);
    let conn = database(&root);
    seed(&conn);
    conn.execute(
        "UPDATE index_runs SET state='Cancelled' WHERE id='run-failed'",
        [],
    )
    .unwrap();
    let b = app.backend();
    let args = json!({"target":"IndexRuns","maxItems":1});
    let direct = fixture::command(&*b, SCHEDULE, args.clone());
    assert!(ops::accepted_decision(b.command(SCHEDULE, &direct)).is_err());
    let definition: JobDefinition = serde_json::from_value(json!({
        "kind":"Native","capabilityId":capability_id(SCHEDULE),
        "arguments":{"schema":argument_schema(SCHEDULE, SchemaRevision(1)).unwrap(),"value":args}
    }))
    .unwrap();
    let create = ops::job_create(
        &*b,
        None,
        "Expire old index runs",
        &Schedule::Manual {},
        &definition,
        true,
    );
    let job = match ops::job_changed(b.command("jobs.create", &create)).unwrap() {
        ops::JobChange::Job(job) => job,
        _ => panic!("expected a job"),
    };
    let frame = ops::job_command(&*b, ops::JobCommand::RunNow, &job);
    let run = match ops::job_changed(b.command("jobs.run_now", &frame)).unwrap() {
        ops::JobChange::Run(run) => run,
        _ => panic!("expected a run"),
    };
    let until = Instant::now() + SETTLE_BOUND;
    let saved = loop {
        let observed = read_snapshot(
            &**app.host().runtime().store(),
            &Default::default(),
            |read| {
                Ok((
                    read.jobs().job_run(&run.id)?.unwrap(),
                    read.retention()?.retention_for_job_run(&run.id)?,
                ))
            },
        )
        .unwrap();
        if observed.0.finished_at_ms.is_some() {
            assert_eq!(observed.0.status, "Succeeded", "{:?}", observed.0);
            let saved = observed.1.unwrap();
            assert_eq!(saved.operation, observed.0.inner_operation_id);
            break saved;
        }
        assert!(Instant::now() < until, "{observed:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(saved.outcome.unwrap().records_removed, 1);
    assert!(!exists(&conn, "index_runs", "run-old"));
    assert!(exists(&conn, "index_runs", "run-failed"));
    let replay = match ops::job_changed(b.command("jobs.run_now", &frame)).unwrap() {
        ops::JobChange::Run(run) => run,
        _ => panic!("expected the original run"),
    };
    assert_eq!(replay.id, run.id);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM operational_retention_previews WHERE native_run_id=?1",
            [run.id.as_str()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn settings_shows_native_preview_and_requires_confirmation() {
    let _case = native_runtime_case();
    let root = Root::new("operational-retention-settings");
    let app = start(&root);
    let conn = database(&root);
    diagnostics(&conn);
    let mut ui = harness(shell(&app, &root, Script::default()), 1440.0, 1040.0);
    settle(&mut ui);
    click_nav(&mut ui, "Settings");
    click(&mut ui, Role::Button, "Review operational retention");
    click(&mut ui, Role::Button, "Preview eligible records");
    assert!(has_text(&ui, "2 selected · 1 held"));
    assert!(exists(&conn, "diagnostic_records", "diag-old"));
    click(&mut ui, Role::CheckBox, "Remove the selected records");
    click(&mut ui, Role::Button, "Confirm removal");
    let until = Instant::now() + SETTLE_BOUND;
    while exists(&conn, "diagnostic_records", "diag-old") {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(exists(&conn, "diagnostic_records", "diag-human"));
}
