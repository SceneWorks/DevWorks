//! Read-only query-shape diagnostic; run outside the measured matrix.
use anyhow::{Result, ensure};
use graphqlite::Graph;
use serde_json::json;
use std::time::Instant;
fn main() -> Result<()> {
    let path = std::path::PathBuf::from(
        std::env::args_os()
            .nth(1)
            .expect("Existing owned GraphQLite workload file required"),
    );
    ensure!(path.is_file());
    let g = Graph::open(&path)?;
    g.query("MATCH (n) RETURN n LIMIT 0")?;
    let raw = g.connection().sqlite_connection();
    let (native,key):(i64,String)=raw.query_row("SELECT n.id,p.value FROM nodes n JOIN node_props_text p ON p.node_id=n.id JOIN property_keys k ON k.id=p.key_id WHERE k.key='id' ORDER BY n.id LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let mut results = Vec::new();
    let mut expected = None;
    for (label, q) in [
        (
            "native_id_function",
            "MATCH (n) WHERE id(n) = $native RETURN n.name AS name",
        ),
        (
            "indexed_external_id_pattern",
            "MATCH (n {id: $external}) RETURN n.name AS name",
        ),
        (
            "external_id_predicate",
            "MATCH (n) WHERE n.id = $external RETURN n.name AS name",
        ),
    ] {
        let params = json!({"native":native,"external":key});
        let explain = format!("EXPLAIN {q}");
        let plan: String = raw.query_row(
            "SELECT cypher(?1,?2)",
            rusqlite::params![explain, params.to_string()],
            |r| r.get(0),
        )?;
        let mut samples = Vec::new();
        for i in 0..2100 {
            let t = Instant::now();
            let r = g
                .query_builder(q)
                .param("native", native)
                .param("external", key.as_str())
                .run()?;
            let ns = t.elapsed().as_nanos() as u64;
            ensure!(r.len() == 1, "Point query cardinality");
            let name = r[0].get::<String>("name")?;
            if let Some(ref n) = expected {
                ensure!(n == &name, "Point shape changed result");
            } else {
                expected = Some(name);
            }
            if i >= 100 {
                samples.push(ns);
            }
        }
        let mut s = samples.clone();
        s.sort_unstable();
        results.push(json!({"shape":label,"query":q,"explain":serde_json::from_str::<serde_json::Value>(&plan).unwrap_or(json!(plan)),"samples":s.len(),"median_ns":s[s.len()/2],"p95_ns":s[((s.len()-1)as f64*0.95).round()as usize],"raw_ns":samples}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"scope":"Repeated fixed point; diagnostic only, not whole-workload ranking","native":native,"external_id":key,"same_result":true,"results":results})
        )?
    );
    Ok(())
}
