use grafeo::{Config, GrafeoDB, Value};
mod adapter;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--workload") {
        return eval_common::workload::run("grafeo", adapter::open);
    }
    let path = eval_common::fresh_probe_path()?;
    let db = GrafeoDB::with_config(Config::persistent(&path))?;
    let s = db.session();
    s.execute("INSERT (:Probe {key: 'a'})")?;
    s.execute("INSERT (:Probe {key: 'b'})")?;
    s.execute("MATCH (a:Probe {key: 'a'}), (b:Probe {key: 'b'}) INSERT (a)-[:LINK]->(b)")?;
    let result = s.execute("MATCH (:Probe)-[:LINK]->(:Probe) RETURN count(*)")?;
    anyhow::ensure!(
        matches!(
            result.rows().first().and_then(|r| r.first()),
            Some(Value::Int64(1))
        ),
        "Unexpected graph result: {:?}",
        result.rows()
    );
    drop(s);
    db.close()?;
    drop(db);
    let db = GrafeoDB::open(&path)?;
    let result = db
        .session()
        .execute("MATCH (:Probe)-[:LINK]->(:Probe) RETURN count(*)")?;
    anyhow::ensure!(
        matches!(
            result.rows().first().and_then(|r| r.first()),
            Some(Value::Int64(1))
        ),
        "Unexpected persisted graph result: {:?}",
        result.rows()
    );
    db.close()?;
    println!(
        "{}",
        serde_json::json!({"engine":"grafeo","version":"0.6.0","open_write_read_reopen":true,"graph_edge":true,"inference":false,"durability":"Persistent config with WAL feature"})
    );
    Ok(())
}
