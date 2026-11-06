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
    StoredInferenceAttempt {
        attempt_id: id(name),
        request_id: id(&format!("request-{name}")),
        operation_id: OperationId(id(&format!("operation-{name}"))),
        attempt_number: 1,
        project_id: Some(ProjectId(id("project"))),
        feature_id: Some(id("feature")),
        work_id: Some(id("feature")),
        assignment_id: None,
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
    assert!(!m.rebuilds_tables);
    let h = Harness::new();
    let old = SqliteStore::open_with(h.config(), &through(27)).unwrap();
    drop(old);
    let s = seeded(&h);
    let a = attempt(&s, "a");
    let expected = a.clone();
    run_write(&s, WriteOptions::default(), move |tx| {
        tx.usage().insert_inference_attempt(&a)?;
        tx.usage()
            .put_attempt_balance(&balance("a", Some(0), Some(100), "OfficialEstimate"), None)
    })
    .unwrap();
    drop(s);
    let old = SqliteStore::open_with(h.config(), &through(27)).unwrap();
    assert!(matches!(
        old.compatibility().unwrap().access,
        AccessMode::ReadOnly { .. }
    ));
    drop(old);
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
