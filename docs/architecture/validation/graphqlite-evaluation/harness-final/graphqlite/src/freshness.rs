//! Raw native CSR observations and an application-generation refresh experiment.
//! This does not implement a production cache service or infer algorithm scope.
use crate::adapter::{bytes, connect};
use anyhow::{Result, ensure};
use graphqlite::Graph;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, time::Instant};
fn stamp(g: &Graph) -> Result<(i64, i64, u64)> {
    let db = g.connection().sqlite_connection();
    Ok((
        db.query_row("SELECT generation FROM eval_generation", [], |r| r.get(0))?,
        db.query_row("PRAGMA data_version", [], |r| r.get(0))?,
        db.total_changes(),
    ))
}
fn bfs(g: &Graph) -> Result<Vec<String>> {
    let mut v = g
        .bfs("a", Some(8))?
        .into_iter()
        .map(|x| x.user_id)
        .collect::<Vec<_>>();
    v.sort();
    v.dedup();
    Ok(v)
}
#[derive(Default)]
struct Guard {
    stamp: Option<(i64, i64, u64)>,
}
impl Guard {
    fn read(&mut self, g: &Graph) -> Result<(Vec<String>, u128, bool)> {
        let db = g.connection().sqlite_connection();
        // The handle is borrowed from a live connection on its owning thread.
        // Serving cached algorithms from a write transaction could publish
        // uncommitted rows; invalidate rather than retain that cache stamp.
        let writing = unsafe {
            rusqlite::ffi::sqlite3_txn_state(db.handle(), std::ptr::null())
                == rusqlite::ffi::SQLITE_TXN_WRITE
        };
        if writing {
            self.stamp = None;
            anyhow::bail!("Managed algorithms require a committed/read-only snapshot");
        }
        let now = stamp(g)?;
        let refresh = self.stamp != Some(now);
        let start = Instant::now();
        if refresh {
            g.reload_graph()?;
            self.stamp = Some(stamp(g)?);
        }
        let result = bfs(g)?;
        Ok((result, start.elapsed().as_nanos(), refresh))
    }
}
fn mutate<F: FnOnce() -> Result<()>>(g: &Graph, f: F) -> Result<()> {
    let db = g.connection().sqlite_connection();
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = f();
    match result {
        Ok(()) => {
            db.execute_batch("UPDATE eval_generation SET generation=generation+1; COMMIT")?;
            Ok(())
        }
        Err(e) => {
            db.execute_batch("ROLLBACK")?;
            Err(e)
        }
    }
}
fn vector(g: &Graph, id: i64) -> Result<()> {
    let mut v = vec![0.; 384];
    v[id as usize % 384] = 1.;
    g.connection().sqlite_connection().execute(
        "INSERT INTO vectors(rowid,embedding,scope) VALUES(?,?,0)",
        rusqlite::params![id, bytes(&v)],
    )?;
    Ok(())
}
fn observe(
    g: &Graph,
    guard: &mut Guard,
    label: &str,
    expected: &[&str],
    observations: &mut Vec<Value>,
    failures: &mut Vec<String>,
) -> Result<()> {
    let expected = expected
        .iter()
        .map(|s| s.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let t = Instant::now();
    let raw = bfs(g)?;
    let raw_ns = t.elapsed().as_nanos();
    // Guard must see the potentially stale native cache before manual reload.
    let (managed, managed_ns, refreshed) = guard.read(g)?;
    let t = Instant::now();
    let refresh = g.reload_graph()?;
    let refresh_ns = t.elapsed().as_nanos();
    let forced = bfs(g)?;
    if forced != expected {
        failures.push(format!(
            "{label}: explicit reload expected {expected:?}, got {forced:?}"
        ));
    }
    if managed != expected {
        failures.push(format!(
            "{label}: generation guard expected {expected:?}, got {managed:?}"
        ));
    }
    observations.push(json!({"phase":label,"expected_bfs":expected,"native_before_reload":raw,"native_was_current":raw==expected,"native_query_ns":raw_ns,"reload_status":refresh,"reload_ns":refresh_ns,"after_explicit_reload":forced,"managed_result":managed,"managed_total_ns":managed_ns,"guard_detected_changed_generation":refreshed,"stamp":stamp(g)?}));
    Ok(())
}
pub fn run() -> Result<()> {
    let path = std::path::PathBuf::from(
        std::env::args_os()
            .nth(2)
            .expect("Fresh owned directory required"),
    );
    ensure!(!path.exists(), "Refusing existing freshness directory");
    std::fs::create_dir_all(&path)?;
    run_at(&path)
}
fn run_at(root: &Path) -> Result<()> {
    let file = root.join("cache.sqlite");
    let g = connect(&file)?;
    let db = g.connection().sqlite_connection();
    db.execute_batch("CREATE TABLE eval_generation(generation INTEGER);INSERT INTO eval_generation VALUES(0);CREATE VIRTUAL TABLE vectors USING vec0(embedding float[384] distance_metric=cosine,scope integer partition key)")?;
    let capabilities: String = db.query_row("SELECT cypher_capabilities()", [], |r| r.get(0))?;
    mutate(&g, || {
        for (id, name) in [(1, "a"), (2, "b"), (4, "d")] {
            g.upsert_node(name, [("name", name)], "Probe")?;
            vector(&g, id)?;
        }
        g.upsert_edge("a", "b", [("weight", 2.0)], "LINK")?;
        Ok(())
    })?;
    let mut guard = Guard::default();
    let mut observations = Vec::new();
    let mut failures = Vec::new();
    let initial_load = g.load_graph()?;
    let baseline = bfs(&g)?;
    ensure!(
        baseline == vec!["a", "b"],
        "Unexpected native BFS baseline {baseline:?}"
    );
    guard.read(&g)?;
    mutate(&g, || {
        g.upsert_node("c", [("name", "C")], "Probe")?;
        g.upsert_edge("b", "c", [("weight", 3.0)], "LINK")?;
        vector(&g, 3)?;
        Ok(())
    })?;
    observe(
        &g,
        &mut guard,
        "same_connection_node_and_edge_insert",
        &["a", "b", "c"],
        &mut observations,
        &mut failures,
    )?;
    let writer = connect(&file)?;
    mutate(&writer, || {
        writer.upsert_edge("c", "d", [("weight", 4.0)], "LINK")?;
        Ok(())
    })?;
    observe(
        &g,
        &mut guard,
        "other_connection_committed_edge",
        &["a", "b", "c", "d"],
        &mut observations,
        &mut failures,
    )?;
    let old_distance = g.shortest_path("a", "d", Some("weight"))?;
    mutate(&writer, || {
        writer.upsert_edge("a", "b", [("weight", 9.0)], "LINK")?;
        Ok(())
    })?;
    let raw_weight = g.shortest_path("a", "d", Some("weight"))?;
    let (_, _, weight_guard_reload) = guard.read(&g)?;
    let guarded_weight = g.shortest_path("a", "d", Some("weight"))?;
    if guarded_weight
        .distance
        .is_none_or(|d| (d - 16.).abs() > 1e-6)
    {
        failures.push(format!("Guarded weighted path mismatch {guarded_weight:?}"));
    }
    g.reload_graph()?;
    let refreshed_weight = g.shortest_path("a", "d", Some("weight"))?;
    if !refreshed_weight.found
        || refreshed_weight
            .distance
            .is_none_or(|d| (d - 16.).abs() > 1e-6)
    {
        failures.push(format!(
            "Refreshed weighted shortest path should be16, got {refreshed_weight:?}"
        ));
    }
    observations.push(json!({"phase":"edge_weight_update","before":old_distance,"without_reload":raw_weight,"after_reload":refreshed_weight,"expected_distance":16,"after_guard":guarded_weight,"guard_reloaded":weight_guard_reload}));
    observe(
        &g,
        &mut guard,
        "edge_weight_generation",
        &["a", "b", "c", "d"],
        &mut observations,
        &mut failures,
    )?;
    mutate(&g, || {
        g.upsert_node("a", [("name", "Renamed")], "Probe")?;
        Ok(())
    })?;
    let names = g
        .query_builder("MATCH (n {id: $id}) RETURN n.name AS name")
        .param("id", "a")
        .run()?;
    ensure!(
        names.len() == 1 && names[0].get::<String>("name")? == "Renamed",
        "Rename did not become visible"
    );
    observe(
        &g,
        &mut guard,
        "node_property_rename",
        &["a", "b", "c", "d"],
        &mut observations,
        &mut failures,
    )?;
    mutate(&g, || {
        g.delete_edge("b", "c", Some("LINK"))?;
        Ok(())
    })?;
    observe(
        &g,
        &mut guard,
        "same_connection_edge_delete",
        &["a", "b"],
        &mut observations,
        &mut failures,
    )?;
    mutate(&writer, || {
        writer.delete_node("b")?;
        writer
            .connection()
            .sqlite_connection()
            .execute("DELETE FROM vectors WHERE rowid=2", [])?;
        Ok(())
    })?;
    observe(
        &g,
        &mut guard,
        "other_connection_node_delete",
        &["a"],
        &mut observations,
        &mut failures,
    )?;
    // Build a cache from uncommitted rows, then roll back. The generation is
    // transactional: its rollback is observable even if total_changes is not.
    db.execute_batch("BEGIN IMMEDIATE")?;
    g.upsert_node("x", [("name", "X")], "Probe")?;
    g.upsert_edge("a", "x", [("weight", 1.0)], "LINK")?;
    vector(&g, 99)?;
    db.execute_batch("UPDATE eval_generation SET generation=generation+1")?;
    g.reload_graph()?;
    let dirty = bfs(&g)?;
    ensure!(
        dirty == vec!["a", "x"],
        "Native uncommitted cache control mismatch"
    );
    ensure!(
        guard.read(&g).is_err(),
        "Managed cache must refuse a write transaction"
    );
    observations.push(json!({"phase":"uncommitted_native_cache_control","native_result":dirty,"managed_write_transaction_rejected":true}));
    db.execute_batch("ROLLBACK")?;
    observe(
        &g,
        &mut guard,
        "rollback_after_caching_uncommitted_rows",
        &["a"],
        &mut observations,
        &mut failures,
    )?;
    ensure!(!g.has_node("x")?, "Rolled back graph node survived");
    let count: i64 = db.query_row("SELECT count(*) FROM vectors WHERE rowid=99", [], |r| {
        r.get(0)
    })?;
    ensure!(count == 0, "Rolled back vector survived");
    writer
        .connection()
        .sqlite_connection()
        .execute_batch("BEGIN IMMEDIATE")?;
    writer.upsert_node("z", [("name", "Z")], "Probe")?;
    writer.upsert_edge("a", "z", [("weight", 1.0)], "LINK")?;
    writer
        .connection()
        .sqlite_connection()
        .execute_batch("UPDATE eval_generation SET generation=generation+1")?;
    observe(
        &g,
        &mut guard,
        "other_writer_uncommitted_not_visible",
        &["a"],
        &mut observations,
        &mut failures,
    )?;
    writer
        .connection()
        .sqlite_connection()
        .execute_batch("ROLLBACK")?;
    observe(
        &g,
        &mut guard,
        "other_writer_rollback",
        &["a"],
        &mut observations,
        &mut failures,
    )?;
    mutate(&g, || {
        g.upsert_node("y", [("name", "Y")], "Probe")?;
        g.upsert_edge("a", "y", [("weight", 1.0)], "LINK")?;
        vector(&g, 51)?;
        Ok(())
    })?;
    observe(
        &g,
        &mut guard,
        "committed_graph_and_vector_transaction",
        &["a", "y"],
        &mut observations,
        &mut failures,
    )?;
    db.execute_batch("BEGIN")?;
    g.reload_graph()?;
    let snap = bfs(&g)?;
    ensure!(snap == vec!["a", "y"]);
    mutate(&writer, || {
        writer.upsert_node("r", [("name", "R")], "Probe")?;
        writer.upsert_edge("y", "r", [("weight", 1.0)], "LINK")?;
        vector(&writer, 53)?;
        Ok(())
    })?;
    ensure!(
        bfs(&g)? == snap,
        "Native cache changed inside pinned read snapshot"
    );
    db.execute_batch("COMMIT")?;
    observe(
        &g,
        &mut guard,
        "read_snapshot_ends_after_other_writer_commit",
        &["a", "r", "y"],
        &mut observations,
        &mut failures,
    )?;
    // Public bulk helpers use direct SQL and own their transactions. Test
    // their cache behavior separately from writes executed through Cypher.
    writer.load_graph()?;
    let mut writer_guard = Guard::default();
    writer_guard.read(&writer)?;
    let ids = writer.insert_nodes_bulk([("bulk", vec![("name", "Bulk")], "Probe")])?;
    writer.insert_edges_bulk([("a", "bulk", Vec::<(&str, &str)>::new(), "LINK")], &ids)?;
    vector(&writer, 77)?;
    let raw_writer_bulk = bfs(&writer)?;
    // A caller cannot rely on data_version for its own writes: all managed
    // writes must advance the transactional generation before cache reuse.
    writer
        .connection()
        .sqlite_connection()
        .execute_batch("UPDATE eval_generation SET generation=generation+1")?;
    let managed_writer_bulk = writer_guard.read(&writer)?;
    if managed_writer_bulk.0 != vec!["a", "bulk", "r", "y"] {
        failures.push(format!(
            "Managed writer cache missed bulk writes {:?}",
            managed_writer_bulk.0
        ));
    }
    observations.push(json!({"phase":"same_connection_public_bulk_helpers","expected_bfs":["a","bulk","r","y"],"native_before_generation_bump":raw_writer_bulk,"managed_result":managed_writer_bulk.0,"guard_reloaded":managed_writer_bulk.2}));
    observe(
        &g,
        &mut guard,
        "other_connection_public_bulk_helpers",
        &["a", "bulk", "r", "y"],
        &mut observations,
        &mut failures,
    )?;
    // ABA control: rollback restores the old generation counter, and a new
    // direct bulk commit can reuse its numeric value without a Cypher mutation
    // invalidating the writer's native CSR cache.
    let before_aba = stamp(&g)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    g.upsert_node("ghost", [("name", "Ghost")], "Probe")?;
    g.upsert_edge("a", "ghost", [("weight", 1.0)], "LINK")?;
    vector(&g, 88)?;
    db.execute_batch("UPDATE eval_generation SET generation=generation+1")?;
    g.reload_graph()?;
    let dirty_aba = bfs(&g)?;
    let dirty_stamp = stamp(&g)?;
    ensure!(
        guard.read(&g).is_err(),
        "Managed cache exposed uncommitted ABA rows"
    );
    db.execute_batch("ROLLBACK")?;
    let ids = g.insert_nodes_bulk([("replacement", vec![("name", "Replacement")], "Probe")])?;
    g.insert_edges_bulk(
        [("d", "replacement", Vec::<(&str, &str)>::new(), "LINK")],
        &ids,
    )?;
    vector(&g, 89)?;
    db.execute_batch("UPDATE eval_generation SET generation=generation+1")?;
    let raw_aba = bfs(&g)?;
    let committed_stamp = stamp(&g)?;
    ensure!(
        dirty_stamp.0 == committed_stamp.0,
        "ABA fixture did not reuse numeric generation"
    );
    let fixed_aba = guard.read(&g)?;
    if fixed_aba.0 != vec!["a", "bulk", "r", "y"] {
        failures.push(format!(
            "Guard failed rollback/new-commit ABA {:?}",
            fixed_aba.0
        ));
    }
    let ghosts: i64 = db.query_row("SELECT count(*) FROM vectors WHERE rowid=88", [], |r| {
        r.get(0)
    })?;
    ensure!(ghosts == 0, "Rolled-back ABA vector survived");
    observations.push(json!({"phase":"rollback_then_new_commit_generation_reuse","before_stamp":before_aba,"dirty_stamp":dirty_stamp,"committed_stamp":committed_stamp,"native_uncommitted":dirty_aba,"native_after_new_commit":raw_aba,"managed_result":fixed_aba.0,"guard_reloaded":fixed_aba.2,"generation_reused":true}));
    let token = stamp(&g)?;
    let (warm, warm_ns, reloaded) = guard.read(&g)?;
    ensure!(
        !reloaded && warm == vec!["a", "bulk", "r", "y"],
        "Guard should reuse unchanged committed cache"
    );
    drop(writer);
    drop(g);
    let reopened = connect(&file)?;
    let reopened_bfs = bfs(&reopened)?;
    if reopened_bfs != vec!["a", "bulk", "r", "y"] {
        failures.push(format!("Uncached reopen BFS mismatch {reopened_bfs:?}"));
    }
    let mut fresh_guard = Guard::default();
    let after_reopen = fresh_guard.read(&reopened)?;
    let vectors: Vec<i64> = reopened
        .connection()
        .sqlite_connection()
        .prepare("SELECT rowid FROM vectors ORDER BY rowid")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        !vectors.contains(&2) && !vectors.contains(&99),
        "Deleted/rolled back vector survived reopen"
    );
    let report = json!({"engine":"graphqlite","version":"0.12.0","sqlite_vec":"0.1.7","capabilities":serde_json::from_str::<Value>(&capabilities)?,"initial_load":initial_load,"baseline_bfs":baseline,"observations":observations,"warm_guard":{"stamp":token,"query_ns":warm_ns,"reloaded":reloaded},"after_reopen":after_reopen.0,"vector_ids_after_reopen":vectors,"case_errors":failures,"real_inference":false,"guard_contract":"All managed graph mutations update a transactional generation; PRAGMA data_version detects other-connection commits; total_changes detects own bulk writes. Write-transaction cache reads are rejected and invalidate the stamp; numeric generation reuse alone is insufficient. Guard must own the cache; scope is whole test database, not a permission boundary."});
    std::fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "{}",
        json!({"output":root.join("result.json"),"errors":report["case_errors"]})
    );
    Ok(())
}
