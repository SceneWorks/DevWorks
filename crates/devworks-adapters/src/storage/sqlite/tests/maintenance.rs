use super::*;
use crate::storage::sqlite::retention_policy::SESSION_FOREIGN_REFERENCES;
use devworks_core::protocol::OperationState;
use devworks_core::protocol::records::PrunePreview;
use devworks_core::protocol::results::OperationResult;
use devworks_core::services::effect::{EffectOutcome, JournaledEffect};
use devworks_core::services::maintenance::{EFFECT_CONSUMER, MaintenanceService};
use std::collections::BTreeSet;

const PRUNE: &str = "maintenance.prune_diagnostics";

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn command_as(
    agent: &InvocationContext,
    store: &SqliteStore,
    service: Arc<MaintenanceService>,
    operation: &str,
    key: &str,
    body: serde_json::Value,
) -> DomainDecision {
    let authority = Granting(Mutex::new(Authorization::direct()));
    let ids = super::seq();
    let ingress = Gateway {
        authority: &authority,
        store,
        ids: &ids,
    };
    let mut body = body;
    body["kind"] = json!(operation);
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":key,"body":body});
    match ingress
        .command(
            agent,
            operation,
            &serde_json::to_vec(&frame).unwrap(),
            service,
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("unexpected receipt {other:?}"),
    }
}

fn command(
    store: &SqliteStore,
    service: Arc<MaintenanceService>,
    operation: &str,
    key: &str,
    body: serde_json::Value,
) -> DomainDecision {
    command_as(&super::ctx(store), store, service, operation, key, body)
}

fn fixture(store: &Arc<SqliteStore>) -> Arc<MaintenanceService> {
    let service = Arc::new(MaintenanceService::new(store.clone()));
    store.with_writer_connection(|c| {
        db(c.execute("INSERT INTO actors VALUES('user-a','human','Workspace owner')",[]))?;
        db(c.execute(
            "INSERT INTO diagnostic_records(id,recorded_at,severity,severity_rank,component,code,message,origin,actor_id)
             VALUES('retention-fixture',1,'Info',0,'retention.test','test','fixture','Host','host')",
            [],
        ))?;
        Ok(())
    }).unwrap();
    service
}

fn preview(store: &Arc<SqliteStore>, service: &Arc<MaintenanceService>) -> PrunePreview {
    match command(
        store,
        service.clone(),
        PRUNE,
        "preview-test",
        json!({"previewOnly":true,"olderThanMs":"2","maxItems":1}),
    ) {
        DomainDecision::Applied {
            result: OperationResult::MaintenancePruneDiagnostics { value },
        } => value,
        other => panic!("unexpected preview {other:?}"),
    }
}

fn queued(
    store: &Arc<SqliteStore>,
    service: &Arc<MaintenanceService>,
    reviewed: &PrunePreview,
) -> OperationId {
    match command(
        store,
        service.clone(),
        PRUNE,
        "apply-test",
        json!({
            "previewOnly":false,"previewId":reviewed.id,"expectedRevision":reviewed.revision,
            "olderThanMs":"2","maxItems":1,
        }),
    ) {
        DomainDecision::Accepted { operation_id } => operation_id,
        other => panic!("unexpected confirmation {other:?}"),
    }
}

fn dispatch(store: &SqliteStore, operation: OperationId) -> OperationRecord {
    write(store, move |tx| {
        let entry = tx
            .queue()
            .claim_ready(&ClaimRequest {
                consumer: EFFECT_CONSUMER.into(),
                owner: id("test-dispatcher"),
                lease_ms: 60_000,
                limit: 1,
            })?
            .pop()
            .unwrap();
        let op = tx.operations().operation(&operation)?.unwrap();
        let attempt = tx.operations().begin_attempt(
            &operation,
            op.revision,
            AttemptBinding {
                owner_generation: Some(entry.generation),
                ..AttemptBinding::default()
            },
        )?;
        tx.operations().transition(
            &operation,
            op.revision + 1,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap()
}

fn row_exists(harness: &Harness) -> bool {
    harness
        .raw()
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM diagnostic_records WHERE id='retention-fixture')",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn every_inbound_session_foreign_key_has_an_explicit_retention_rule() {
    let h = Harness::new();
    let _store = h.store();
    let conn = h.raw();
    let expected: BTreeSet<(String, String)> = SESSION_FOREIGN_REFERENCES
        .iter()
        .map(|(table, column)| ((*table).into(), (*column).into()))
        .collect();
    let mut sql = conn
        .prepare(
            "SELECT s.name,f.[from] FROM sqlite_schema s, pragma_foreign_key_list(s.name) f
         WHERE s.type='table' AND f.[table]='sessions'",
        )
        .unwrap();
    let actual = sql
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<BTreeSet<(String, String)>, _>>()
        .unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn no_deletion_survives_a_failed_result_write_and_a_lost_ack_observes_the_committed_batch_after_restart()
 {
    let h = Harness::new();
    let store = Arc::new(h.store());
    let service = fixture(&store);
    let reviewed = preview(&store, &service);
    let operation = dispatch(&store, queued(&store, &service, &reviewed));
    let mut stale = operation.clone();
    stale.attempts.last_mut().unwrap().binding.owner_generation = Some(-1);
    assert!(matches!(service.execute(&stale), EffectOutcome::Unknown(_)));
    assert!(row_exists(&h));

    store
        .with_writer_connection(|c| {
            db(c.execute_batch(
        "CREATE TRIGGER retention_test_error BEFORE UPDATE ON operational_retention_previews
         WHEN json_extract(NEW.record_json,'$.phase')='Applied' BEGIN
           SELECT raise(ABORT,'test cannot write outcome');
         END;"
    ))
        })
        .unwrap();
    assert!(matches!(
        service.execute(&operation),
        EffectOutcome::Unknown(_)
    ));
    assert!(row_exists(&h));
    assert!(matches!(
        service.observe(&operation),
        EffectOutcome::NotApplied(_)
    ));
    store
        .with_writer_connection(|c| db(c.execute_batch("DROP TRIGGER retention_test_error")))
        .unwrap();

    store.lose_next_ack();
    assert!(matches!(
        service.execute(&operation),
        EffectOutcome::Unknown(_)
    ));
    assert!(!row_exists(&h));
    drop(service);
    drop(store);
    let restarted = Arc::new(h.store());
    let restored = MaintenanceService::new(restarted.clone());
    let result = restored.observe(&operation);
    assert!(matches!(result, EffectOutcome::Completed(_)), "{result:?}");
    let saved = read_snapshot(&*restarted, &ReadOptions::default(), |read| {
        Ok(read
            .retention()?
            .retention_for_operation(&operation.id)?
            .unwrap())
    })
    .unwrap();
    assert_eq!(saved.outcome.unwrap().records_removed, 1);
    assert_eq!(restored.execute(&operation), result);
}

#[test]
fn a_manual_cancellation_before_the_worker_claims_keeps_the_selected_record() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    let service = fixture(&store);
    let reviewed = preview(&store, &service);
    let operation = queued(&store, &service, &reviewed);
    let result = command(
        &store,
        service,
        "maintenance.cancel",
        "cancel-test",
        json!({
            "previewId":reviewed.id,"expectedRevision":"2",
        }),
    );
    assert!(
        matches!(result,DomainDecision::Applied { result: OperationResult::MaintenanceCancel { value } }
        if matches!(value.phase,devworks_core::protocol::records::PrunePhase::Cancelled))
    );
    assert!(row_exists(&h));
    assert_eq!(
        read_snapshot(&*store, &ReadOptions::default(), |read| Ok(read
            .operations()
            .operation(&operation)?
            .unwrap()
            .state))
        .unwrap(),
        OperationState::CancelledBeforeDispatch,
    );
}

#[test]
fn expired_previews_and_previews_owned_by_another_human_cannot_be_confirmed() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    let service = fixture(&store);
    let reviewed = preview(&store, &service);
    let input = json!({
        "previewOnly":false,"previewId":reviewed.id,"expectedRevision":reviewed.revision,
        "olderThanMs":"2","maxItems":1,
    });
    store
        .with_writer_connection(|c| {
            db(c.execute(
                "INSERT INTO actors VALUES('other-human','human','Other author')",
                [],
            ))
            .map(|_| ())
        })
        .unwrap();
    let other = devworks_core::context::HostIngress::new(&*store).context(
        ActorId(id("other-human")),
        PrincipalKind::Human,
        id("other-inv"),
        id("ui-main"),
        Counter::new(store.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let outcome = command_as(
        &other,
        &store,
        service.clone(),
        PRUNE,
        "other-human-apply",
        input.clone(),
    );
    assert!(matches!(outcome,DomainDecision::Refused { fault }
        if fault.code == devworks_core::protocol::DomainErrorCode::NotFound));
    h.set_now(reviewed.expires_at_ms.get());
    let outcome = command(&store, service, PRUNE, "expired-apply", input);
    assert!(matches!(outcome,DomainDecision::Refused { fault }
        if fault.code == devworks_core::protocol::DomainErrorCode::SourceChanged));
    assert!(row_exists(&h));
}
