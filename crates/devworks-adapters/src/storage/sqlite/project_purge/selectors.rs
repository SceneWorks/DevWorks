//! Qualification of the explicit native scope/lineage candidates, before mixed-owner
//! partitioning is allowed to produce any exact-row deletion permit.
#[cfg(test)]
mod tests {
    #[test]
    fn native_candidate_queries_resolve_against_the_migrated_schema_and_do_not_mutate_it() {
        let policy: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/architecture/validation/project-purge/native-row-selectors.json"
        )))
        .unwrap();
        let h = crate::storage::sqlite::tests::Harness::new();
        drop(h.store());
        let c = h.raw();
        let anchors = policy["anchors"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, query)| format!("{name} AS ({})", query.as_str().unwrap()))
            .collect::<Vec<_>>()
            .join(",");
        let schema_before: i64 = c
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        for (table, predicate) in policy["candidatePredicates"].as_object().unwrap() {
            let sql = format!(
                "WITH {anchors} SELECT count(*) FROM \"{table}\" WHERE ({})",
                predicate.as_str().unwrap()
            );
            let count: i64 = c
                .query_row(&sql, ["no-project-owned-rows"], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{table}: {e}"));
            assert_eq!(count, 0, "{table}");
        }
        assert_eq!(
            c.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            schema_before
        );
    }
}
