pub mod core;
mod sqlite;
use core::*;
use sqlite::SqliteStore;
use std::path::Path;
pub fn run(path: &Path) -> std::result::Result<serde_json::Value, String> {
    let mut store = SqliteStore::open(path).map_err(|e| format!("{e:?}"))?;
    store.install_fixture().map_err(|e| format!("{e:?}"))?;
    let result = (|| -> core::Result<serde_json::Value> {
        let mut checks = Vec::new();
        macro_rules! check {
            ($name:literal,$condition:expr) => {
                if !$condition {
                    return Err(StoreError::Unavailable);
                }
                checks.push($name);
            };
        }
        check!(
            "effective_wal_full_and_foreign_keys",
            store.effective_settings()? == ("wal".into(), 2, 1)
        );
        check!(
            "transaction_drop_rolls_back_domain_receipt_history_outbox",
            rename_project(&mut store, "failed", "p", 1, "Uncommitted", true)
                == Err(StoreError::Interrupted)
                && store.fixture_title()? == "Before"
                && store.fixture_counts()? == (0, 0, 0)
        );
        check!(
            "committed_uow_changes_all_repositories",
            rename_project(&mut store, "rename", "p", 1, "After", false)?
                && store.fixture_title()? == "After"
                && store.fixture_counts()? == (1, 1, 1)
        );
        check!(
            "receipt_replay_precedes_revision_check",
            !rename_project(&mut store, "rename", "p", 1, "After", false)?
                && store.fixture_counts()? == (1, 1, 1)
        );
        check!(
            "different_payload_same_key_is_conflict",
            rename_project(&mut store, "rename", "p", 1, "Other", false)
                == Err(StoreError::ReceiptConflict)
        );
        check!(
            "revision_conflict_leaves_all_unchanged",
            rename_project(&mut store, "stale", "p", 1, "Other", false)
                == Err(StoreError::RevisionConflict)
                && store.fixture_counts()? == (1, 1, 1)
        );
        let scope = Scope {
            project_id: "p".into(),
            expected_scope_revision: 1,
        };
        let base = store.pin(scope.clone(), "view-base", 1, 100)?;
        let current = store.pin(scope.clone(), "view-capture", 1, 100)?;
        check!(
            "baseline_view_retains_its_own_nodes",
            store.nodes(&base, 1)? == vec!["a-old", "b-old", "keep"]
        );
        check!(
            "highest_unit_replacement_and_delete_mask_lower_layers",
            store.nodes(&current, 1)? == vec!["b-new", "keep"]
        );
        check!(
            "masked_producers_and_missing_endpoints_do_not_return_edges",
            store.edges(&current, 1)? == vec![("b-new".into(), "keep".into())]
        );
        check!(
            "unauthorized_source_view_is_denied",
            matches!(
                store.pin(scope.clone(), "view-private", 1, 100),
                Err(StoreError::ScopeDenied)
            )
        );
        check!(
            "mixed_source_generation_is_denied",
            matches!(
                store.pin(scope.clone(), "view-mixed", 1, 100),
                Err(StoreError::ViewUnavailable)
            )
        );
        check!(
            "missing_view_does_not_fall_back",
            matches!(
                store.pin(scope.clone(), "missing", 1, 100),
                Err(StoreError::ViewUnavailable)
            )
        );
        let candidates = store.vector_candidates(&current, &[0., 0.], "two", 1, 1)?;
        check!(
            "authorized_visible_vectors_are_ranked_before_top_k",
            candidates.len() == 1
                && candidates[0].chunk_id == "b-new"
                && (candidates[0].distance - 1.).abs() < 1e-6
        );
        check!(
            "wrong_query_dimension_rejected",
            matches!(
                store.vector_candidates(&current, &[0., 0., 0.], "two", 1, 1),
                Err(StoreError::Unavailable)
            )
        );
        check!(
            "nonfinite_query_vector_rejected",
            matches!(
                store.vector_candidates(&current, &[f32::NAN, 0.], "two", 1, 1),
                Err(StoreError::Unavailable)
            )
        );
        check!(
            "private_fts_rows_not_materialized",
            store.lexical_candidates(&current, "private", 1)?.is_empty()
        );
        check!(
            "masked_fts_rows_not_materialized",
            store
                .lexical_candidates(&current, "outdated", 1)?
                .is_empty()
        );
        check!(
            "current_fts_rows_remain_searchable",
            store.lexical_candidates(&current, "current", 1)? == vec!["b-new"]
        );
        store.fixture_advance_source()?;
        let counts = store.fixture_counts()?;
        check!(
            "stale_builder_cannot_publish",
            store.publish("repo-a", 1, 1, "view-capture") == Err(StoreError::RevisionConflict)
                && store.fixture_counts()? == counts
        );
        store.publish("repo-a", 2, 1, "view-capture")?;
        check!(
            "published_change_commits_event_and_delivery",
            store.fixture_counts()? == (1, 2, 2)
        );
        check!(
            "pointer_compare_and_swap_rejects_late_builder",
            store.publish("repo-a", 2, 1, "view-base") == Err(StoreError::RevisionConflict)
        );
        check!(
            "old_pin_preserves_old_view_after_publication",
            store.nodes(&base, 2)? == vec!["a-old", "b-old", "keep"]
                && !store.fixture_view_reclaimable("view-base", 2)?
        );
        check!(
            "expired_pin_cannot_keep_reading",
            matches!(store.nodes(&base, 101), Err(StoreError::ViewUnavailable))
        );
        check!(
            "expired_unpublished_view_can_be_considered_for_gc",
            store.fixture_view_reclaimable("view-base", 101)?
                && !store.fixture_view_reclaimable("view-capture", 101)?
        );
        let other = SqliteStore::open(path)?;
        other.fixture_revoke()?;
        check!(
            "access_revocation_before_snippet_publication_discards_candidates",
            matches!(
                store.materialize(&current, &candidates, 2),
                Err(StoreError::ScopeDenied)
            )
        );
        let (sqlite, vector) = store.versions()?;
        Ok(
            serde_json::json!({"date":"2026-11-01","status":"Native driver-free port/SQLite fixture only; production adapter and general authority not implemented","checks":checks,"passed":checks.len(),"sqliteVersion":sqlite,"sqliteVecVersion":vector,"noInference":true,"scope":"Core transaction lifetime, atomic receipts/events/outbox, whole-unit overlays, scoped FTS/vector, publication CAS/pins and membership-revocation fixture"}),
        )
    })();
    result.map_err(|e| format!("native probe assertion/storage error: {e:?}"))
}
