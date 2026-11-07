//! Canonical accounting persistence: no provider calls or inference execution.
use super::{Harness, T0, through};
use crate::storage::sqlite::{MIGRATION_0028_SHA256, MIGRATIONS, SqliteStore};
use devworks_core::ports::storage::*;
use devworks_core::ports::usage::PublishedTariff;
use devworks_core::protocol::{Id, OperationId, ProjectId};
use serde_json::json;
use std::sync::{Arc, Barrier};
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
fn seeded(h: &Harness) -> SqliteStore {
    let s = h.store();
    let actor = s.info().host_actor;
    run_write(&s, WriteOptions::default(), move |tx| {
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
        for (name, kind) in [("feature", FEATURE), ("issue", TASK)] {
            tx.work().insert_work(&StoredWork {
                id: id(name),
                project_id: ProjectId(id("project")),
                kind: kind.into(),
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
                brief: json!({}),
            })?;
        }
        tx.router().put_connection(
            &StoredConnection {
                id: id("connection"),
                name: "Explicit connection".into(),
                profile: "CodexSubscriptionResponses".into(),
                endpoint: "https://example.test".into(),
                enabled: true,
                local_free: false,
                open_router: None,
                credential_ref: None,
                credential_revision: 0,
                account_revision: 1,
                refresh_operation_id: None,
                credential_epoch: None,
                lifecycle: "Disconnected".into(),
                state_reason: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: None,
                checked_at_ms: None,
                revision: 1,
                created_at_ms: T0,
                updated_at_ms: T0,
            },
            None,
        )?;
        assert_eq!(tx.store().host_actor, actor);
        Ok(())
    })
    .unwrap();
    s
}
fn attempt(s: &SqliteStore, name: &str) -> StoredInferenceAttempt {
    // A real durable journal+logical request fixture. Canonical accounting never invents these.
    let mut raw = rusqlite::Connection::open(&s.path).unwrap();
    raw.pragma_update(None, "foreign_keys", true).unwrap();
    let tx = raw.transaction().unwrap();
    tx.execute("INSERT INTO operations(id,root_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES(?1,'feature','inference.request',1,?2,'{}','Preflighted',2,?3)",rusqlite::params![format!("operation-{name}"),"a".repeat(64),T0]).unwrap();
    tx.execute("INSERT INTO operation_attempts(id,operation_id,attempt_no,boot_id,state,observations_json) VALUES(?1,?2,1,?3,'Preflighted','[]')",rusqlite::params![name,format!("operation-{name}"),s.info().boot_id.as_ref().unwrap().as_str()]).unwrap();
    tx.execute("INSERT INTO router_requests(request_id,operation_id,purpose,project_id,model_id,connection_id,created_at) VALUES(?1,?2,'Conversation','project','sol','connection',?3)",rusqlite::params![format!("request-{name}"),format!("operation-{name}"),T0]).unwrap();
    tx.commit().unwrap();

    StoredInferenceAttempt {
        attempt_id: id(name),
        request_id: id(&format!("request-{name}")),
        operation_id: OperationId(id(&format!("operation-{name}"))),
        attempt_number: 1,
        project_id: Some(ProjectId(id("project"))),
        feature_id: Some(id("feature")),
        work_id: Some(id("feature")),
        assignment_id: None,
        execution_root_id: None,
        execution_id: None,
        session_id: None,
        job_run_id: None,
        actor_id: s.info().host_actor,
        role: "FeatureCoordinator".into(),
        purpose: "Conversation".into(),
        catalog_model_id: id("sol"),
        connection_id: id("connection"),
        endpoint_profile: "CodexSubscriptionResponses".into(),
        access_mode: "Subscription".into(),
        provider_model_id: None,
        pricing_snapshot: json!({"quote":{"classification":"Untracked","connection_revision":1,"override_revision":null,"snapshot_id":null},"reservation_micro_usd":100}),
        state: "Admitted".into(),
        revision: 1,
        created_at_ms: T0,
    }
}
fn balance(
    name: &str,
    priced: Option<i64>,
    reserved: Option<i64>,
    class: &str,
) -> StoredAttemptBalance {
    StoredAttemptBalance {
        attempt_id: id(name),
        revision: 1,
        priced_micro_usd: priced,
        reserved_micro_usd: reserved,
        classification: class.into(),
        monetary_complete: priced.is_some(),
        incomplete: false,
        unknown_tail: false,
        local_quiescent: false,
        summary: json!({"merged":"exact host facts"}),
    }
}
fn origin(name: &str, kind: &str, amount: Option<i64>) -> StoredUsageOrigin {
    StoredUsageOrigin {
        source_kind: kind.into(),
        source_id: name.into(),
        attempt_id: None,
        request_id: None,
        project_id: Some(ProjectId(id("project"))),
        feature_id: Some(id("feature")),
        work_id: None,
        session_id: None,
        job_run_id: None,
        payload: json!({"historical":"attribution retained"}),
        recorded_at_ms: T0,
        priced_micro_usd: amount,
        classification: if amount.is_some() {
            "OfficialEstimate"
        } else {
            "Untracked"
        }
        .into(),
        monetary_complete: amount.is_some(),
        incomplete: false,
        unknown_tail: false,
    }
}
fn totals(s: &SqliteStore) -> StoredFeatureUsageTotals {
    read_snapshot(s, &ReadOptions::default(), |r| {
        r.usage().feature_usage_totals(&id("feature"))
    })
    .unwrap()
}
fn tariff() -> PublishedTariff {
    PublishedTariff {
        upstream_model_id: "gpt-6.1-sol".into(),
        service_tier: "Standard".into(),
        context_lower_exclusive: None,
        context_upper_inclusive: None,
        modality: "Text".into(),
        charge_kind: "Input".into(),
        cache_ttl_seconds: None,
        unit: "Token".into(),
        units: 1_000_000,
        amount_usd: "1.25".into(),
        region: None,
        endpoint_profile: None,
    }
}
fn pending() -> StoredPriceStatus {
    StoredPriceStatus {
        provider: "OpenAI".into(),
        revision: 1,
        current_snapshot_id: None,
        last_attempt_ms: Some(T0),
        last_success_ms: None,
        refresh_pending: true,
        refresh_operation_id: None,
        last_error: None,
        next_refresh_ms: None,
    }
}
fn publication() -> StoredPriceSnapshot {
    StoredPriceSnapshot {
        id: id("snapshot"),
        provider: "OpenAI".into(),
        revision: 1,
        source_url: "https://openai.com/api/pricing/".into(),
        source_digest: "a".repeat(64),
        parser_version: "fixture1".into(),
        fetched_at_ms: T0,
        verified_at_ms: T0,
        effective_date: None,
        tariffs: vec![tariff()],
    }
}
#[test]
fn usage28_migration_is_checksummed_old_writers_refused_and_new_records_reopen() {
    let m = MIGRATIONS.iter().find(|m| m.version == 28).unwrap();
    assert_eq!(m.checksum(), MIGRATION_0028_SHA256);
    assert_eq!((m.minimum_reader, m.minimum_writer), (8, 28));
    assert!(m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(27)).unwrap();
    run_write(&old, WriteOptions::default(), |tx| {
        tx.operations().record_intent(NewOperation {
            id: OperationId(id("legacy-operation")),
            root_id: None,
            assignment_id: None,
            capability: "inference.request".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        tx.router().record_route_request(&StoredRouteRequest {
            request_id: id("legacy-request"),
            operation_id: OperationId(id("legacy-operation")),
            purpose: "Conversation".into(),
            session_id: None,
            project_id: None,
            model_id: id("sol"),
            connection_id: id("legacy-connection"),
            pin_id: None,
            created_at_ms: T0,
        })
    })
    .unwrap();
    drop(old);
    // Isolate this migration's writer floor before later reader-floor migrations.
    drop(SqliteStore::open_with(h.config(), &through(28)).unwrap());
    let old = SqliteStore::open_with(h.config(), &through(27)).unwrap();
    assert!(matches!(
        old.compatibility().unwrap().access,
        AccessMode::ReadOnly { .. }
    ));
    drop(old);
    let s = seeded(&h);
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let route = r.router().route_request(&id("legacy-request"))?.unwrap();
        assert_eq!(route.connection_id, id("legacy-connection"));
        assert!(
            r.usage()
                .inference_attempts(&UsageAttemptFilter::default(), None, 500)?
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
    let a = attempt(&s, "a");
    let expected = a.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage()
            .put_attempt_balance(&balance("a", Some(0), Some(100), "OfficialEstimate"), None)
    })
    .unwrap();
    drop(s);
    let s = h.store();
    assert_eq!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .usage()
            .inference_attempt(&id("a")))
        .unwrap(),
        Some(expected)
    );
    assert_eq!(totals(&s).spent_micro_usd, 0);
    assert_eq!(totals(&s).reserved_micro_usd, 100);
    assert!(s.integrity_check().unwrap().problems.is_empty());
}
#[test]
fn usage28_balances_replace_estimate_without_double_counting_and_legacy_facts_stay_separate() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "a");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage()
            .put_attempt_balance(&balance("a", Some(60), Some(40), "OfficialEstimate"), None)?;
        tx.usage()
            .insert_usage_origin(&origin("legacy", "SessionUsage", Some(900)))?;
        tx.usage()
            .insert_usage_origin(&origin("tool", "ToolInvocation", Some(800)))?;
        tx.usage()
            .insert_usage_origin(&origin("request", "RouterRequest", None))?;
        Ok(())
    })
    .unwrap();
    let t = totals(&s);
    assert_eq!(
        (
            t.attempts,
            t.spent_micro_usd,
            t.reserved_micro_usd,
            t.legacy_usage_facts,
            t.legacy_observation_micro_usd
        ),
        (1, 60, 40, 1, 900)
    );
    run_write(&s, WriteOptions::default(), |tx| {
        let mut b = balance("a", Some(75), Some(0), "ReportedCharge");
        b.revision = 2;
        tx.usage().put_attempt_balance(&b, Some(1))?;
        assert!(tx.usage().put_attempt_balance(&b, Some(1)).is_err());
        tx.usage()
            .bind_usage_origin("SessionUsage", "legacy", &id("a"))
    })
    .unwrap();
    let t = totals(&s);
    assert_eq!(
        (
            t.spent_micro_usd,
            t.reported_micro_usd,
            t.estimated_micro_usd,
            t.reserved_micro_usd,
            t.legacy_usage_facts
        ),
        (75, 75, 0, 0, 0)
    );
}
#[test]
fn usage28_observations_are_immutable_idempotent_bounded_and_attempt_binding_cannot_change() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "a");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        for n in 1..=3 {
            let o = StoredUsageObservation {
                attempt_id: id("a"),
                observation_key: format!("chunk-{n}"),
                sequence: n,
                digest: "a".repeat(64),
                payload: json!({"n":n}),
                recorded_at_ms: T0,
            };
            tx.usage().insert_usage_observation(&o)?;
            tx.usage().insert_usage_observation(&o)?;
            let mut bad = o;
            bad.payload = json!({"tampered":true});
            assert!(tx.usage().insert_usage_observation(&bad).is_err());
        }
        let mut bad = a.clone();
        bad.revision = 2;
        bad.project_id = None;
        assert!(tx.usage().update_inference_attempt(&bad, 1).is_err());
        let mut dispatched = a;
        dispatched.revision = 2;
        dispatched.state = "Dispatched".into();
        tx.usage().update_inference_attempt(&dispatched, 1)?;
        assert!(tx.usage().update_inference_attempt(&dispatched, 1).is_err());
        let quote = dispatched.pricing_snapshot.clone();
        dispatched.provider_model_id = Some("provider-alias".into());
        dispatched.revision = 3;
        tx.usage().update_inference_attempt(&dispatched, 2)?;
        dispatched.provider_model_id = Some("provider-concrete-version".into());
        dispatched.revision = 4;
        tx.usage().update_inference_attempt(&dispatched, 3)?;
        assert_eq!(
            tx.usage()
                .inference_attempt(&dispatched.attempt_id)?
                .unwrap()
                .pricing_snapshot,
            quote
        );
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let p = r.usage().usage_observations(&id("a"), Some(1), 1)?;
        assert_eq!(p[0].sequence, 2);
        assert!(r.usage().usage_observations(&id("a"), None, 0).is_err());
        assert!(
            r.usage()
                .inference_attempts(&UsageAttemptFilter::default(), None, 501)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    assert!(
        h.raw()
            .execute("UPDATE usage_observations SET sequence=9", [])
            .is_err()
    );
}
#[test]
fn usage28_price_publication_override_history_and_connection_revision_fences_are_atomic() {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        assert_eq!(tx.usage().price_revision()?, 1);
        assert_eq!(tx.usage().price_override_revision(&id("sol"))?, 1);
        tx.usage().put_price_status(&pending(), None)?;
        assert_eq!(tx.usage().price_revision()?, 2);
        tx.usage().publish_price_snapshot(&publication(), 1)?;
        assert_eq!(tx.usage().price_revision()?, 3);
        assert!(
            tx.usage()
                .publish_price_snapshot(&publication(), 1)
                .is_err()
        );
        assert_eq!(tx.usage().price_revision()?, 3);
        let set = StoredPriceOverride {
            model_id: id("sol"),
            revision: 2,
            mode: "Set".into(),
            tariffs: vec![tariff()],
            updated_by: tx.store().host_actor.clone(),
            updated_at_ms: T0,
        };
        tx.usage().put_price_override(&set, 1)?;
        let mut clear = set;
        clear.revision = 3;
        clear.mode = "Clear".into();
        clear.tariffs.clear();
        tx.usage().put_price_override(&clear, 2)?;
        assert_eq!(tx.usage().price_revision()?, 5);
        Ok(())
    })
    .unwrap();
    let a = attempt(&s, "stale-override");
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .usage()
            .insert_inference_attempt(&a))
        .is_err()
    );
    let mut a = attempt(&s, "stale-connection");
    a.pricing_snapshot["quote"]["override_revision"] = json!(3);
    run_write(&s, WriteOptions::default(), |tx| {
        let mut c = tx.router().connection(&id("connection"))?.unwrap();
        c.local_free = true;
        tx.router().put_connection(&c, Some(1))?;
        Ok(())
    })
    .unwrap();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .usage()
            .insert_inference_attempt(&a))
        .is_err()
    );
    let mut a = attempt(&s, "free");
    a.pricing_snapshot["quote"] =
        json!({"classification":"LocalFree","connection_revision":2,"override_revision":3});
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage()
            .put_attempt_balance(&balance("free", Some(0), Some(0), "LocalFree"), None)
    })
    .unwrap();
    assert_eq!(totals(&s).local_free_attempts, 1);
    assert_eq!(
        h.raw()
            .query_row("SELECT count(*) FROM usage_price_overrides", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn usage28_concurrent_budget_cas_has_one_winner_and_only_features_own_caps() {
    let h = Harness::new();
    let s = Arc::new(seeded(&h));
    run_write(s.as_ref(), WriteOptions::default(), |tx| {
        let row = StoredFeatureBudget {
            feature_id: id("feature"),
            revision: 1,
            limit_micro_usd: Some(100),
            warning_revision: None,
        };
        tx.usage().put_feature_budget(&row, None)?;
        let mut issue = row;
        issue.feature_id = id("issue");
        assert!(tx.usage().put_feature_budget(&issue, None).is_err());
        Ok(())
    })
    .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|n| {
            let s = s.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                run_write(s.as_ref(), WriteOptions::default(), move |tx| {
                    tx.usage().put_feature_budget(
                        &StoredFeatureBudget {
                            feature_id: id("feature"),
                            revision: 2,
                            limit_micro_usd: Some(200 + n),
                            warning_revision: None,
                        },
                        Some(1),
                    )
                })
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
}
#[test]
fn usage28_bootstrap_checkpoint_restart_and_origin_paging_preserve_unknown_identity() {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        for name in ["a", "b", "c"] {
            tx.usage()
                .insert_usage_origin(&origin(name, "SessionUsage", None))?;
        }
        let mut b = tx.usage().usage_bootstrap()?;
        assert_eq!((b.revision, b.phase.as_str()), (1, "Sessions"));
        b.revision = 2;
        b.phase = "Jobs".into();
        b.processed = 3;
        tx.usage().update_usage_bootstrap(&b, 1)?;
        assert!(tx.usage().update_usage_bootstrap(&b, 1).is_err());
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let b = r.usage().usage_bootstrap()?;
        assert_eq!((b.revision, b.phase.as_str(), b.processed), (2, "Jobs", 3));
        let p = r
            .usage()
            .usage_origins(&UsageOriginFilter::default(), None, 1)?;
        assert_eq!(p[0].source_id, "a");
        assert!(p[0].attempt_id.is_none());
        let p = r
            .usage()
            .usage_origins(&UsageOriginFilter::default(), Some(&p[0].cursor()), 1)?;
        assert_eq!(p[0].source_id, "b");
        assert!(
            r.usage()
                .usage_origins(&UsageOriginFilter::default(), Some("bad"), 1)
                .is_err()
        );
        Ok(())
    })
    .unwrap();
}
#[test]
fn usage28_pending_partial_and_tail_coverage_are_distinct_and_integer_totals_refuse_overflow() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "pending");
    let mut b = attempt(&s, "partial");
    b.request_id = id("request-partial");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage().insert_inference_attempt(&b)?;
        let mut partial = balance("partial", Some(5), Some(10), "OfficialEstimate");
        partial.monetary_complete = false;
        partial.unknown_tail = true;
        tx.usage().put_attempt_balance(&partial, None)
    })
    .unwrap();
    let t = totals(&s);
    assert_eq!(
        (
            t.attempts,
            t.spent_micro_usd,
            t.untracked_attempts,
            t.monetary_incomplete_attempts,
            t.unknown_tail_attempts,
            t.incomplete_attempts
        ),
        (2, 5, 0, 2, 1, 1)
    );
    run_write(&s, WriteOptions::default(), |tx| {
        tx.usage().put_attempt_balance(
            &balance("pending", Some(i64::MAX), None, "ReportedCharge"),
            None,
        )
    })
    .unwrap();
    let error = read_snapshot(&s, &ReadOptions::default(), |r| {
        r.usage().feature_usage_totals(&id("feature"))
    })
    .unwrap_err();
    assert!(matches!(error, StorageError::InvalidRecord(_)));
}
#[test]
fn usage28_origin_binding_is_cross_project_guarded_and_exact_replay_preserves_prior_binding() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "a");
    let b = attempt(&s, "b");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        let original = origin("legacy", "SessionUsage", Some(5));
        tx.usage().insert_usage_origin(&original)?;
        tx.usage()
            .bind_usage_origin("SessionUsage", "legacy", &id("a"))?;
        tx.usage().insert_usage_origin(&original)?;
        assert_eq!(
            tx.usage()
                .usage_origin("SessionUsage", "legacy")?
                .unwrap()
                .attempt_id,
            Some(id("a"))
        );
        let mut bad = original;
        bad.payload = json!({"rewrite":true});
        assert!(tx.usage().insert_usage_origin(&bad).is_err());
        tx.usage().insert_inference_attempt(&b)?;
        assert!(
            tx.usage()
                .bind_usage_origin("SessionUsage", "legacy", &id("b"))
                .is_err()
        );
        let mut bad = origin("bad-scope", "SessionUsage", None);
        bad.project_id = Some(ProjectId(id("wrong-project")));
        assert!(tx.usage().insert_usage_origin(&bad).is_err());
        Ok(())
    })
    .unwrap();
    assert!(h.raw().execute("DELETE FROM usage_origins", []).is_err());
}
#[test]
fn usage28_independent_provider_publications_ignore_unrelated_global_drift_and_stale_worker_keeps_last_good()
 {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        tx.usage().put_price_status(&pending(), None)?;
        let mut other = pending();
        other.provider = "Anthropic".into();
        tx.usage().put_price_status(&other, None)?;
        assert_eq!(tx.usage().price_revision()?, 3);
        let published = tx.usage().publish_price_snapshot(&publication(), 1)?;
        assert_eq!(published.current_snapshot_id, Some(id("snapshot")));
        let mut refresh = published;
        refresh.revision = 3;
        refresh.refresh_pending = true;
        tx.usage().put_price_status(&refresh, Some(2))?;
        let mut replacement = refresh;
        replacement.revision = 4;
        tx.usage().put_price_status(&replacement, Some(3))?;
        let mut stale = publication();
        stale.id = id("stale-snapshot");
        stale.revision = 2;
        let generation = tx.usage().price_revision()?;
        assert!(tx.usage().publish_price_snapshot(&stale, 3).is_err());
        assert!(tx.usage().price_snapshot(&stale.id)?.is_none());
        assert_eq!(tx.usage().price_revision()?, generation);
        assert_eq!(
            tx.usage().current_price_snapshot("OpenAI")?.unwrap().id,
            id("snapshot")
        );
        Ok(())
    })
    .unwrap();
}
#[test]
fn usage28_concurrent_price_refresh_generation_has_one_winner() {
    let h = Harness::new();
    let s = Arc::new(seeded(&h));
    let b = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let s = s.clone();
            let b = b.clone();
            std::thread::spawn(move || {
                b.wait();
                run_write(s.as_ref(), WriteOptions::default(), |tx| {
                    tx.usage().advance_price_revision(1)
                })
            })
        })
        .collect();
    b.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(StorageError::RevisionConflict { .. })))
    );
}
#[test]
fn usage28_global_session_bootstrap_reads_project_and_workspace_facts_even_after_tombstone() {
    use devworks_core::protocol::EventId;
    let h = Harness::new();
    let s = seeded(&h);
    let actor = s.info().host_actor;
    run_write(&s, WriteOptions::default(), move |tx| {
        for (name, project) in [
            ("project-session", Some(ProjectId(id("project")))),
            ("workspace-session", None),
        ] {
            tx.sessions().insert_session(&StoredSession {
                id: id(name),
                conversation_id: id(&format!("conversation-{name}")),
                agent_id: actor.clone(),
                project_id: project.clone(),
                root_id: None,
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
            let event = id(&format!("event-{name}"));
            let sequence = tx.history().append(NewEvent {
                id: EventId(event.clone()),
                actor: actor.clone(),
                project_id: project.clone(),
                root_id: None,
                operation_id: None,
                aggregate_kind: "session".into(),
                aggregate_id: name.into(),
                aggregate_revision: 1,
                kind: "UsageRecorded".into(),
                payload: json!({}),
            })?;
            let batch = id(&format!("batch-{name}"));
            tx.sessions().insert_batch(&StoredBatch {
                batch_id: batch.clone(),
                session_id: id(name),
                payload_digest: "a".repeat(64),
                actor: actor.clone(),
                context_epoch: 0,
                first_sequence: 1,
                last_sequence: 1,
                event_id: event,
                committed_sequence: sequence,
                accepted_at_ms: T0,
            })?;
            tx.sessions().insert_usage(&StoredUsage {
                session_id: id(name),
                batch_id: batch,
                ordinal: 1,
                project_id: project,
                root_id: None,
                committed_sequence: sequence,
                item: json!({"requestId":format!("request-{name}")}),
            })?;
        }
        Ok(())
    })
    .unwrap();
    h.raw()
        .execute(
            "UPDATE projects SET lifecycle='Deleted' WHERE id='project'",
            [],
        )
        .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let scoped = r
            .sessions()
            .usage_facts(&HistoryScope::Workspace, None, 500)?;
        assert_eq!(scoped.len(), 1);
        let first = r.sessions().all_usage_facts(None, 1)?;
        assert_eq!(first.len(), 1);
        let second = r.sessions().all_usage_facts(Some(first[0].key()), 1)?;
        assert_eq!(second.len(), 1);
        assert_ne!(first[0].key(), second[0].key());
        let all = r.sessions().all_usage_facts(None, 500)?;
        assert_eq!(all.len(), 2);
        assert!(r.sessions().all_usage_facts(None, 0).is_err());
        Ok(())
    })
    .unwrap();
    let raw = h.raw();
    for (table, index) in [
        ("session_usage", "session_usage_global"),
        ("tool_invocations", "tool_invocation_global"),
    ] {
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT * FROM {table} WHERE committed_seq>=?1 ORDER BY committed_seq LIMIT 1"
        );
        let detail: String = raw.query_row(&sql, [0], |r| r.get(3)).unwrap();
        assert!(detail.contains(index), "{detail}");
    }
}
#[test]
fn usage28_canonical_identity_requires_actual_matching_journal_and_request_without_partial_rows() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "a");
    let b = attempt(&s, "b");
    for (n, mut invalid) in [
        ("fabricated", a.clone()),
        ("number", a.clone()),
        ("operation", a.clone()),
        ("request", a.clone()),
        ("root", a.clone()),
    ] {
        match n {
            "fabricated" => invalid.attempt_id = id("fabricated"),
            "number" => invalid.attempt_number = 2,
            "operation" => invalid.operation_id = b.operation_id.clone(),
            "request" => invalid.request_id = b.request_id.clone(),
            _ => {
                invalid.feature_id = None;
                invalid.work_id = Some(id("issue"));
            }
        }
        assert!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .usage()
                .insert_inference_attempt(&invalid))
            .is_err()
        );
    }
    assert_eq!(totals(&s).attempts, 0);
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)
    })
    .unwrap();
    assert!(
        h.raw()
            .execute("UPDATE usage_attempts SET role='rewritten'", [])
            .is_err()
    );
    assert!(h.raw().execute("DELETE FROM usage_attempts", []).is_err());
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let one = r.router().route_requests(None, 1)?;
        assert_eq!(one[0].request_id, id("request-a"));
        let next = r.router().route_requests(Some(&one[0].request_id), 1)?;
        assert_eq!(next[0].request_id, id("request-b"));
        assert!(r.router().route_requests(None, 0).is_err());
        assert!(r.usage().inference_attempt(&id("b"))?.is_none());
        Ok(())
    })
    .unwrap();
}
#[test]
fn usage28_global_job_facts_page_by_run_and_sequence_without_current_scope_inference() {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        for (name, project) in [("job-a", Some(ProjectId(id("project")))), ("job-b", None)] {
            let job = StoredJob {
                id: id(name),
                revision: 1,
                project_id: project.clone(),
                title: name.into(),
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
            };
            tx.jobs().insert_job(&job)?;
            let operation = OperationId(id(&format!("operation-{name}")));
            tx.operations().record_intent(NewOperation {
                id: operation.clone(),
                root_id: None,
                assignment_id: None,
                capability: "jobs.run".into(),
                schema_version: 1,
                args_digest: "a".repeat(64),
                payload: json!({}),
            })?;
            let run = StoredJobRun {
                id: id(&format!("run-{name}")),
                job_id: id(name),
                seq: 1,
                project_id: project,
                job_revision: 1,
                definition: "{}".into(),
                project_epoch: 0,
                restore_epoch: 0,
                occurrence_key: name.into(),
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
            };
            tx.jobs().insert_run(&run)?;
            for n in 1..=2 {
                tx.jobs().record_usage(&StoredJobUsage {
                    run_id: run.id.clone(),
                    seq: n,
                    request_id: id(&format!("request-{name}-{n}")),
                    purpose: "Conversation".into(),
                    model_id: id("sol"),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    amount_usd: None,
                    price_kind: "Untracked".into(),
                    price_origin: "Unknown".into(),
                    price_revision: None,
                    incomplete: true,
                    recorded_at_ms: T0,
                })?;
            }
        }
        Ok(())
    })
    .unwrap();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let first = r.jobs().usage_history(None, 1)?;
        assert_eq!((first[0].run_id.as_str(), first[0].seq), ("run-job-a", 1));
        let second = r.jobs().usage_history(
            Some(&UsageJobPosition {
                run_id: first[0].run_id.clone(),
                seq: first[0].seq,
            }),
            2,
        )?;
        assert_eq!((second[0].run_id.as_str(), second[0].seq), ("run-job-a", 2));
        assert_eq!((second[1].run_id.as_str(), second[1].seq), ("run-job-b", 1));
        assert!(r.jobs().usage_history(None, 501).is_err());
        assert!(
            r.jobs()
                .usage_history(
                    Some(&UsageJobPosition {
                        run_id: id("run-job-a"),
                        seq: -1
                    }),
                    1
                )
                .is_err()
        );
        Ok(())
    })
    .unwrap();
    // A job which dispatches actual feature work is secondary provenance, not a second expense.
    let mut owned = attempt(&s, "feature-job");
    owned.job_run_id = Some(id("run-job-b"));
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&owned)?;
        tx.usage().put_attempt_balance(
            &balance("feature-job", Some(25), None, "ReportedCharge"),
            None,
        )
    })
    .unwrap();
    let t = totals(&s);
    assert_eq!((t.attempts, t.spent_micro_usd), (1, 25));
    let mut mismatch = attempt(&s, "wrong-job-project");
    mismatch.project_id = None;
    mismatch.feature_id = None;
    mismatch.work_id = None;
    mismatch.job_run_id = Some(id("run-job-a"));
    let raw = h.raw();
    raw.execute(
        "UPDATE operations SET root_id=NULL WHERE id='operation-wrong-job-project'",
        [],
    )
    .unwrap();
    raw.execute(
        "UPDATE router_requests SET project_id=NULL WHERE request_id='request-wrong-job-project'",
        [],
    )
    .unwrap();
    assert!(
        run_write(&s, WriteOptions::default(), move |tx| tx
            .usage()
            .insert_inference_attempt(&mismatch))
        .is_err()
    );
}
#[test]
fn usage28_host_decision_purposes_are_closed_and_preserve_router_identity() {
    let h = Harness::new();
    let s = seeded(&h);
    let a = attempt(&s, "decision");
    let b = attempt(&s, "tier");
    let raw = h.raw();
    for (request, purpose) in [
        (a.request_id.as_str(), "Decision"),
        (b.request_id.as_str(), "TierClassification"),
    ] {
        raw.execute(
            "UPDATE router_requests SET purpose=?1 WHERE request_id=?2",
            rusqlite::params![purpose, request],
        )
        .unwrap();
    }
    assert!(
        raw.execute("UPDATE router_requests SET purpose='ArbitraryTool'", [])
            .is_err()
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        let mut a = a;
        a.purpose = "Decision".into();
        let mut b = b;
        b.purpose = "TierClassification".into();
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage().insert_inference_attempt(&b)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(totals(&s).attempts, 2);
    run_write(&s, WriteOptions::default(), |tx| {
        let operation = OperationId(id("host-purpose-operation"));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "inference.request".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        tx.router().record_route_request(&StoredRouteRequest {
            request_id: id("host-purpose-request"),
            operation_id: operation,
            purpose: "Decision".into(),
            session_id: None,
            project_id: None,
            model_id: id("sol"),
            connection_id: id("connection"),
            pin_id: None,
            created_at_ms: T0,
        })
    })
    .unwrap();
    assert!(s.integrity_check().unwrap().problems.is_empty());
}
#[test]
fn usage28_price_history_is_bounded_and_refresh_operation_survives_publication_failure_and_restart()
{
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        let operation = OperationId(id("refresh-operation"));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: "usage.price_refresh".into(),
            schema_version: 1,
            args_digest: "a".repeat(64),
            payload: json!({}),
        })?;
        let mut first = pending();
        first.refresh_operation_id = Some(operation.clone());
        tx.usage().put_price_status(&first, None)?;
        let published = tx.usage().publish_price_snapshot(&publication(), 1)?;
        assert_eq!(published.refresh_operation_id, Some(operation.clone()));
        let mut refresh = published;
        refresh.revision = 3;
        refresh.refresh_pending = true;
        tx.usage().put_price_status(&refresh, Some(2))?;
        let mut second = publication();
        second.id = id("snapshot-z");
        second.revision = 2;
        tx.usage().publish_price_snapshot(&second, 3)?;
        let mut failed = tx.usage().price_status("OpenAI")?.unwrap();
        failed.revision = 5;
        failed.last_error = Some("safe failure".into());
        tx.usage().put_price_status(&failed, Some(4))?;
        failed.revision = 6;
        failed.refresh_operation_id = None;
        assert!(tx.usage().put_price_status(&failed, Some(5)).is_err());
        let mut other = pending();
        other.provider = "Anthropic".into();
        tx.usage().put_price_status(&other, None)?;
        let mut other = publication();
        other.id = id("snapshot-other");
        other.provider = "Anthropic".into();
        tx.usage().publish_price_snapshot(&other, 1)?;
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let status = r.usage().price_status("OpenAI")?.unwrap();
        assert_eq!(
            status.refresh_operation_id,
            Some(OperationId(id("refresh-operation")))
        );
        assert_eq!(status.current_snapshot_id, Some(id("snapshot-z")));
        let first = r.usage().price_snapshots(Some("OpenAI"), None, 1)?;
        assert_eq!(first[0].id, id("snapshot"));
        let next = r
            .usage()
            .price_snapshots(Some("OpenAI"), Some(&first[0].id), 1)?;
        assert_eq!(next[0].id, id("snapshot-z"));
        assert_eq!(r.usage().price_snapshots(None, None, 500)?.len(), 3);
        assert!(r.usage().price_snapshots(None, None, 0).is_err());
        assert!(r.usage().price_snapshots(None, None, 501).is_err());
        Ok(())
    })
    .unwrap();
    assert!(s.integrity_check().unwrap().problems.is_empty());
}
fn managed_source(s: &SqliteStore, name: &str, worker: bool) -> StoredInferenceAttempt {
    use devworks_core::protocol::{AssignmentId, RootId};
    let mut a = attempt(s, name);
    let actor = s.info().host_actor;
    let boot = s.info().boot_id.unwrap();
    let owned_name = name.to_owned();
    run_write(s, WriteOptions::default(), move |tx| {
        let name = owned_name.as_str();
        if tx.runs().activation(&id("execution"))?.is_none() {
            tx.runs().insert_activation(&StoredActivation {
                id: id("execution"),
                root_id: RootId(id("feature")),
                boot_id: boot,
                generation: 1,
                phase: ActivationPhase::Running,
                purpose: ActivationPurpose::Planning,
                owner: None,
                lease_until_ms: None,
                owner_epoch: 1,
                revision: 1,
                attempts: 0,
                ready_at_ms: T0,
                created_at_ms: T0,
                stop_cause: None,
                stop_requested_at_ms: None,
                stop_acknowledged_at_ms: None,
                stopped_at_ms: None,
                outcome: None,
                detail: None,
                plan_revision: None,
            })?;
        }
        tx.sessions().insert_session(&StoredSession {
            id: id(name),
            conversation_id: id(&format!("conversation-{name}")),
            agent_id: actor.clone(),
            project_id: Some(ProjectId(id("project"))),
            root_id: Some(RootId(id("feature"))),
            title: name.into(),
            context_epoch: 0,
            revision: 1,
            created_by: actor,
            created_at_ms: T0,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        if worker {
            tx.runs().insert_assignment(&StoredAssignment {
                purpose: devworks_core::ports::storage::AssignmentPurpose::Primary,
                id: AssignmentId(id(name)),
                root_id: RootId(id("feature")),
                work_id: RootId(id("feature")),
                parent_id: None,
                session_id: id(name),
                execution_id: Some(id("execution")),
                state: AssignmentState::Superseded,
                generation: 1,
                slot_held: false,
                revision: 1,
                checkpoint: None,
                scope_key: None,
                scope_digest: None,
                plan_revision: None,
                instruction_revision: 0,
                created_at_ms: T0,
            })?;
        }
        Ok(())
    })
    .unwrap();
    let raw = rusqlite::Connection::open(&s.path).unwrap();
    raw.execute(
        "UPDATE router_requests SET session_id=?1 WHERE request_id=?2",
        rusqlite::params![name, a.request_id.as_str()],
    )
    .unwrap();
    a.session_id = Some(id(name));
    a.assignment_id = worker.then(|| AssignmentId(id(name)));
    a.execution_root_id = Some(RootId(id("feature")));
    a.execution_id = Some(id("execution"));
    a
}
#[test]
fn usage28_captured_coordinator_custody_survives_execution_stop_and_restart_without_assignment_or_cost_inference()
 {
    use devworks_core::protocol::RootId;
    let h = Harness::new();
    let s = seeded(&h);
    let manual = attempt(&s, "manual");
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&manual)?;
        tx.usage()
            .put_attempt_balance(&balance("manual", None, None, "Untracked"), None)
    })
    .unwrap();
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .inference_quiescent(&RootId(id("feature"))))
        .unwrap()
    );
    let a = managed_source(&s, "coordinator", false);
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        let mut b = balance("coordinator", Some(3), Some(100), "OfficialEstimate");
        b.unknown_tail = true;
        b.monetary_complete = false;
        tx.usage().put_attempt_balance(&b, None)
    })
    .unwrap();
    assert!(
        !read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .inference_quiescent(&RootId(id("feature"))))
        .unwrap()
    );
    h.raw()
        .execute(
            "UPDATE executions SET phase='Stopped',outcome='Interrupted',stopped_at=1700000000000 WHERE id='execution'",
            [],
        )
        .unwrap();
    drop(s);
    let s = h.store();
    assert!(
        !read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .inference_quiescent(&RootId(id("feature"))))
        .unwrap()
    );
    run_write(&s, WriteOptions::default(), |tx| {
        let mut b = tx.usage().attempt_balance(&id("coordinator"))?.unwrap();
        b.revision = 2;
        b.local_quiescent = true;
        tx.usage().put_attempt_balance(&b, Some(1))?;
        b.revision = 3;
        b.local_quiescent = false;
        assert!(tx.usage().put_attempt_balance(&b, Some(2)).is_err());
        Ok(())
    })
    .unwrap();
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .inference_quiescent(&RootId(id("feature"))))
        .unwrap()
    );
    let t = totals(&s);
    assert_eq!(
        (
            t.spent_micro_usd,
            t.unknown_tail_attempts,
            t.monetary_incomplete_attempts
        ),
        (3, 1, 2)
    );
}
#[test]
fn usage28_superseded_assignment_handoff_waits_for_canonical_transport_even_without_operation_assignment()
 {
    use devworks_core::protocol::{AssignmentId, RootId};
    let h = Harness::new();
    let s = seeded(&h);
    let a = managed_source(&s, "worker", true);
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .assignment_handoff_quiescent(&AssignmentId(id("worker"))))
        .unwrap()
    );
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)
    })
    .unwrap();
    // A missing balance is conservatively unclosed; operation.assignment_id is intentionally NULL.
    assert!(
        !read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .assignment_handoff_quiescent(&AssignmentId(id("worker"))))
        .unwrap()
    );
    run_write(&s, WriteOptions::default(), |tx| {
        let mut b = balance("worker", None, None, "Untracked");
        b.local_quiescent = true;
        b.unknown_tail = true;
        tx.usage().put_attempt_balance(&b, None)
    })
    .unwrap();
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .assignment_handoff_quiescent(&AssignmentId(id("worker"))))
        .unwrap()
    );
    assert!(
        read_snapshot(&s, &ReadOptions::default(), |r| r
            .runs()
            .inference_quiescent(&RootId(id("feature"))))
        .unwrap()
    );
}
#[test]
fn usage28_execution_owner_pair_is_validated_and_immutable() {
    use devworks_core::protocol::RootId;
    let h = Harness::new();
    let s = seeded(&h);
    let valid = managed_source(&s, "owned", false);
    for mode in 0..3 {
        let mut bad = valid.clone();
        match mode {
            0 => bad.execution_id = None,
            1 => bad.execution_root_id = Some(RootId(id("issue"))),
            _ => bad.session_id = None,
        };
        assert!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .usage()
                .insert_inference_attempt(&bad))
            .is_err()
        );
    }
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&valid)?;
        let mut bad = valid;
        bad.revision = 2;
        bad.execution_root_id = None;
        bad.execution_id = None;
        assert!(tx.usage().update_inference_attempt(&bad, 1).is_err());
        Ok(())
    })
    .unwrap();
    assert!(h.raw().execute("UPDATE usage_attempts SET execution_id=NULL,execution_root_id=NULL WHERE attempt_id='owned'",[]).is_err());
}
#[test]
fn usage28_override_history_retains_set_a_set_b_clear_and_pages_across_models_after_restart() {
    let h = Harness::new();
    let s = seeded(&h);
    run_write(&s, WriteOptions::default(), |tx| {
        let actor = tx.store().host_actor.clone();
        let mut a = StoredPriceOverride {
            model_id: id("sol"),
            revision: 2,
            mode: "Set".into(),
            tariffs: vec![tariff()],
            updated_by: actor.clone(),
            updated_at_ms: T0,
        };
        tx.usage().put_price_override(&a, 1)?;
        a.revision = 3;
        a.tariffs[0].amount_usd = "2.75".into();
        tx.usage().put_price_override(&a, 2)?;
        a.revision = 4;
        a.mode = "Clear".into();
        a.tariffs.clear();
        tx.usage().put_price_override(&a, 3)?;
        let other = StoredPriceOverride {
            model_id: id("opus"),
            revision: 2,
            mode: "Set".into(),
            tariffs: vec![tariff()],
            updated_by: actor,
            updated_at_ms: T0,
        };
        tx.usage().put_price_override(&other, 1)?;
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let rows = r
            .usage()
            .price_override_history(Some(&id("sol")), None, 500)?;
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (
                rows[0].revision,
                rows[0].mode.as_str(),
                rows[0].tariffs[0].amount_usd.as_str()
            ),
            (2, "Set", "1.25")
        );
        assert_eq!(
            (
                rows[1].revision,
                rows[1].mode.as_str(),
                rows[1].tariffs[0].amount_usd.as_str()
            ),
            (3, "Set", "2.75")
        );
        assert_eq!((rows[2].revision, rows[2].mode.as_str()), (4, "Clear"));
        assert!(rows[2].tariffs.is_empty());
        assert_eq!(r.usage().price_override(&id("sol"))?.unwrap(), rows[2]);
        let first = r.usage().price_override_history(None, None, 1)?;
        assert_eq!(first[0].model_id, id("opus"));
        let cursor = PriceOverridePosition {
            model_id: first[0].model_id.clone(),
            revision: first[0].revision,
        };
        let next = r.usage().price_override_history(None, Some(&cursor), 1)?;
        assert_eq!((next[0].model_id.as_str(), next[0].revision), ("sol", 2));
        let cursor = PriceOverridePosition {
            model_id: id("sol"),
            revision: 2,
        };
        let next = r
            .usage()
            .price_override_history(Some(&id("sol")), Some(&cursor), 1)?;
        assert_eq!(next[0].revision, 3);
        assert!(r.usage().price_override_history(None, None, 0).is_err());
        assert!(r.usage().price_override_history(None, None, 501).is_err());
        assert!(
            r.usage()
                .price_override_history(
                    None,
                    Some(&PriceOverridePosition {
                        model_id: id("sol"),
                        revision: 0
                    }),
                    1
                )
                .is_err()
        );
        assert!(
            r.usage()
                .price_override_history(Some(&id("missing")), None, 1)?
                .is_empty()
        );
        Ok(())
    })
    .unwrap();
    assert!(s.integrity_check().unwrap().problems.is_empty());
}
#[test]
fn usage28_provider_model_identity_uses_retained_text_contract_without_byte_or_newline_narrowing() {
    let h = Harness::new();
    let s = seeded(&h);
    let ascii = "x".repeat(513);
    let unicode = format!("{}\n", "界".repeat(4095));
    assert_eq!(unicode.chars().count(), 4096);
    assert!(unicode.len() > 4096);
    let mut a = attempt(&s, "served-identity");
    a.provider_model_id = Some(ascii.clone());
    a.pricing_snapshot["quoted_provider_model_id"] = json!(ascii.clone());
    let quoted = a.pricing_snapshot.clone();
    let mut original = a.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)
    })
    .unwrap();
    original.revision = 2;
    original.state = "Dispatched".into();
    original.provider_model_id = Some(unicode.clone());
    let observed = original.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().update_inference_attempt(&observed, 1)
    })
    .unwrap();
    let mut ascii_only = attempt(&s, "ascii-identity");
    ascii_only.provider_model_id = Some(ascii.clone());
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&ascii_only)
    })
    .unwrap();
    let invalid_admission = attempt(&s, "invalid-identity");
    for invalid in ["x".repeat(4097), "model\0identity".into(), String::new()] {
        let mut update = original.clone();
        update.revision = 3;
        update.provider_model_id = Some(invalid.clone());
        assert!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .usage()
                .update_inference_attempt(&update, 2))
            .is_err()
        );
        let mut insert = invalid_admission.clone();
        insert.provider_model_id = Some(invalid.clone());
        assert!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .usage()
                .insert_inference_attempt(&insert))
            .is_err()
        );
        let mut insert = invalid_admission.clone();
        insert.pricing_snapshot["quoted_provider_model_id"] = json!(invalid);
        assert!(
            run_write(&s, WriteOptions::default(), move |tx| tx
                .usage()
                .insert_inference_attempt(&insert))
            .is_err()
        );
    }
    let mut a = attempt(&s, "quoted-unicode");
    a.provider_model_id = Some(unicode.clone());
    a.pricing_snapshot["quoted_provider_model_id"] = json!(unicode.clone());
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let a = r
            .usage()
            .inference_attempt(&id("served-identity"))?
            .unwrap();
        assert_eq!(a.provider_model_id, Some(unicode.clone()));
        assert_eq!(a.revision, 2);
        assert_eq!(a.pricing_snapshot, quoted);
        assert_eq!(a.pricing_snapshot["quoted_provider_model_id"], json!(ascii));
        assert_eq!(
            r.usage()
                .inference_attempt(&id("ascii-identity"))?
                .unwrap()
                .provider_model_id,
            Some(ascii.clone())
        );
        let a = r.usage().inference_attempt(&id("quoted-unicode"))?.unwrap();
        assert_eq!(
            a.pricing_snapshot["quoted_provider_model_id"],
            json!(unicode)
        );
        assert!(
            r.usage()
                .inference_attempt(&id("invalid-identity"))?
                .is_none()
        );
        Ok(())
    })
    .unwrap();
}
#[test]
fn usage28_quoted_tariff_upstream_identity_preserves_full_text_contract() {
    let h = Harness::new();
    let s = seeded(&h);
    let unicode = format!("{}\n", "界".repeat(4095));
    let expected = unicode.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        let mut row = StoredPriceOverride {
            model_id: id("sol"),
            revision: 2,
            mode: "Set".into(),
            tariffs: vec![tariff()],
            updated_by: tx.store().host_actor.clone(),
            updated_at_ms: T0,
        };
        row.tariffs[0].upstream_model_id = unicode;
        tx.usage().put_price_override(&row, 1)?;
        for invalid in ["x".repeat(4097), "upstream\0identity".into()] {
            row.revision = 3;
            row.tariffs[0].upstream_model_id = invalid;
            assert!(tx.usage().put_price_override(&row, 2).is_err());
        }
        Ok(())
    })
    .unwrap();
    drop(s);
    let s = h.store();
    read_snapshot(&s, &ReadOptions::default(), |r| {
        let row = r.usage().price_override(&id("sol"))?.unwrap();
        assert_eq!(row.revision, 2);
        assert_eq!(row.tariffs[0].upstream_model_id, expected);
        Ok(())
    })
    .unwrap();
}
