//! Whole-database native CSR timings. These are not project-scoped query timings.
use crate::adapter::{bytes, connect};
use anyhow::{Result, ensure};
use eval_common::workload::{Dataset, RunConfig, deletion_targets, subset};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};
fn reach(edges: &HashMap<u64, Vec<u64>>, root: u64, depth: usize) -> Vec<u64> {
    let mut seen = HashSet::from([root]);
    let mut frontier = vec![root];
    for _ in 0..depth {
        let mut next = Vec::new();
        for n in frontier {
            for id in edges.get(&n).into_iter().flatten() {
                if seen.insert(*id) {
                    next.push(*id);
                }
            }
        }
        frontier = next;
    }
    let mut out = seen.into_iter().collect::<Vec<_>>();
    out.sort_unstable();
    out
}
fn native(g: &graphqlite::Graph, root: u64) -> Result<Vec<u64>> {
    let mut out = g
        .bfs(&format!("n{root}"), Some(3))?
        .into_iter()
        .map(|r| {
            r.user_id
                .strip_prefix('n')
                .ok_or_else(|| anyhow::anyhow!("Unexpected user ID {}", r.user_id))
                .and_then(|v| v.parse().map_err(Into::into))
        })
        .collect::<Result<Vec<u64>>>()?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}
fn rss() -> Option<u64> {
    String::from_utf8(
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?
            .stdout,
    )
    .ok()?
    .trim()
    .parse()
    .ok()
}
pub fn run() -> Result<()> {
    let cfg: RunConfig = serde_json::from_reader(std::fs::File::open(
        std::env::args_os().nth(2).expect("Config path required"),
    )?)?;
    ensure!(
        cfg.database.join("store.sqlite").is_file(),
        "Existing owned workload database required"
    );
    ensure!(
        !cfg.output.exists(),
        "Refusing existing cache-profile report"
    );
    let data: Dataset =
        serde_json::from_reader(std::io::BufReader::new(std::fs::File::open(&cfg.fixture)?))?;
    let mut data = subset(data, cfg.limit, cfg.overlay);
    let deleted = deletion_targets(&data)
        .into_iter()
        .map(|n| n.id)
        .collect::<HashSet<_>>();
    data.nodes.retain(|n| !deleted.contains(&n.id));
    data.edges
        .retain(|e| !deleted.contains(&e.src) && !deleted.contains(&e.dst));
    let mut adjacency = HashMap::<u64, Vec<u64>>::new();
    let mut degrees = data
        .nodes
        .iter()
        .map(|n| (n.id, (0_i64, 0_i64)))
        .collect::<HashMap<_, _>>();
    for e in &data.edges {
        adjacency.entry(e.src).or_default().push(e.dst);
        degrees.get_mut(&e.src).unwrap().1 += 1;
        degrees.get_mut(&e.dst).unwrap().0 += 1;
    }
    let g = connect(&cfg.database.join("store.sqlite"))?;
    let db = g.connection().sqlite_connection();
    db.execute_batch("CREATE TABLE IF NOT EXISTS eval_generation(generation INTEGER);INSERT INTO eval_generation SELECT 0 WHERE NOT EXISTS(SELECT 1 FROM eval_generation)")?;
    let baseline_rss = rss();
    let t = Instant::now();
    let loaded = g.load_graph()?;
    let load_ns = t.elapsed().as_nanos();
    let loaded_rss = rss();
    ensure!(
        loaded.node_count == Some(data.nodes.len() as i64)
            && loaded.edge_count == Some(data.edges.len() as i64),
        "CSR cardinality mismatch: {loaded:?}"
    );
    let mut checks = 2;
    let mut failures = Vec::<String>::new();
    let mut raw = Vec::new();
    for i in 0..80 {
        let n = &data.nodes[(i * 7919 + 17) % data.nodes.len()];
        let expected = reach(&adjacency, n.id, 3);
        let t = Instant::now();
        let got = native(&g, n.id)?;
        raw.push(t.elapsed().as_nanos() as u64);
        if got != expected {
            failures.push(format!(
                "Native whole-db BFS mismatch root={}: expected {} ids, got {}",
                n.id,
                expected.len(),
                got.len()
            ));
        }
        checks += 1;
    }
    let t = Instant::now();
    let actual = g.degree_centrality()?;
    let degree_ns = t.elapsed().as_nanos();
    let mut visited = HashSet::new();
    for row in &actual {
        let id: u64 = row
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Native degree result omitted external ID"))?
            .strip_prefix('n')
            .ok_or_else(|| anyhow::anyhow!("Bad degree user id"))?
            .parse()?;
        let &(inc, out) = degrees
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("Unexpected degree node"))?;
        visited.insert(id);
        if row.in_degree != inc || row.out_degree != out || row.degree != inc + out {
            failures.push(format!(
                "Native degree mismatch gid={id}: {row:?}; expected in={inc},out={out}"
            ));
        }
        checks += 1;
    }
    if visited.len() != degrees.len() {
        failures.push(format!(
            "Native degree omitted nodes: {} of {}",
            visited.len(),
            degrees.len()
        ));
    }
    let root = data.nodes[0].id;
    let mut expected = reach(&adjacency, root, 3);
    let extra = 1_u64 << 50;
    let key = format!("n{extra}");
    let writer = connect(&cfg.database.join("store.sqlite"))?;
    let w = writer.connection().sqlite_connection();
    w.execute_batch("BEGIN IMMEDIATE")?;
    writer.upsert_node(&key, [("name", "cache profile temporary node")], "Symbol")?;
    writer.upsert_edge(
        &format!("n{root}"),
        &key,
        Vec::<(&str, &str)>::new(),
        "EVAL",
    )?;
    w.execute(
        "INSERT INTO vectors(rowid,embedding,scope)VALUES(?,?,?)",
        rusqlite::params![
            extra as i64,
            bytes(&data.nodes[0].vector),
            data.nodes[0].scope().code()
        ],
    )?;
    w.execute_batch("UPDATE eval_generation SET generation=generation+1;COMMIT")?;
    let without_reload = native(&g, root)?;
    expected.push(extra);
    expected.sort_unstable();
    let t = Instant::now();
    let added = g.reload_graph()?;
    let add_reload_ns = t.elapsed().as_nanos();
    let after_reload = native(&g, root)?;
    if after_reload != expected {
        failures.push(
            "Whole-db cache stale after explicit reload following other-connection insertion"
                .into(),
        );
    }
    checks += 1;
    w.execute_batch("BEGIN IMMEDIATE")?;
    writer.delete_node(&key)?;
    w.execute("DELETE FROM vectors WHERE rowid=?", [extra as i64])?;
    w.execute_batch("UPDATE eval_generation SET generation=generation+1;COMMIT")?;
    let t = Instant::now();
    let restored = g.reload_graph()?;
    let delete_reload_ns = t.elapsed().as_nanos();
    if native(&g, root)? != reach(&adjacency, root, 3) {
        failures.push("Whole-db cache stale after explicit reload following deletion".into());
    }
    let vector_count: i64 = db.query_row(
        "SELECT count(*) FROM vectors WHERE rowid=?",
        [extra as i64],
        |r| r.get(0),
    )?;
    ensure!(vector_count == 0, "Temporary vector not removed");
    checks += 2;
    let mut sorted = raw.clone();
    sorted.sort_unstable();
    let summary = json!({"samples":sorted.len(),"median_ns":sorted[sorted.len()/2],"p95_ns":sorted[((sorted.len()-1)as f64*0.95).round()as usize],"p99_ns":null});
    let report = json!({"engine":"graphqlite","scope":"whole-database native algorithms; not a project/view permission boundary","nodes":data.nodes.len(),"edges":data.edges.len(),"load_status":loaded,"initial_load_ns":load_ns,"baseline_rss_kib":baseline_rss,"loaded_rss_kib":loaded_rss,"warm_bfs_summary":summary,"warm_bfs_raw_ns":raw,"degree_total_ns":degree_ns,"degree_nodes":actual.len(),"other_connection_insert":{"without_reload_is_fresh":without_reload==expected,"after_reload_correct":after_reload==expected,"reload_ns":add_reload_ns,"load_status":added},"after_delete_reload_ns":delete_reload_ns,"restored_status":restored,"correctness_checks":checks,"case_errors":failures,"real_inference":false});
    if let Some(p) = cfg.output.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&cfg.output, serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        json!({"output":cfg.output,"errors":report["case_errors"]})
    );
    Ok(())
}
